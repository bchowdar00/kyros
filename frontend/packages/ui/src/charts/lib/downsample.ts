import type { ChartPoint } from './types'

/**
 * M4 downsampling: keeps the first, lowest, highest and last sample of every
 * horizontal bucket so spikes survive while the path stays small.
 */
export function downsampleM4(
  points: readonly ChartPoint[],
  toX: (time: number) => number,
  bucketWidth = 1
): readonly ChartPoint[] {
  if (points.length < 8) {
    return points
  }

  const result: ChartPoint[] = []
  let bucketIndex = Number.NaN
  let first: ChartPoint | undefined
  let last: ChartPoint | undefined
  let min: ChartPoint | undefined
  let max: ChartPoint | undefined

  const flush = () => {
    if (!first || !last || !min || !max) {
      return
    }

    const middle = min.time <= max.time ? [min, max] : [max, min]

    for (const point of [first, ...middle, last]) {
      if (result.at(-1) !== point) {
        result.push(point)
      }
    }
  }

  for (const point of points) {
    const nextBucket = Math.floor(toX(point.time) / bucketWidth)

    if (nextBucket !== bucketIndex) {
      flush()
      bucketIndex = nextBucket
      first = point
      min = point
      max = point
    } else {
      if (min && point.value < min.value) {
        min = point
      }

      if (max && point.value > max.value) {
        max = point
      }
    }

    last = point
  }

  flush()

  return result.length < points.length ? result : points
}

/**
 * Averages a dense series into `buckets` evenly spaced time slots. Unlike LTTB,
 * which keeps the most extreme sample of each bucket, this drops the tick noise
 * that makes a one-second feed render as hair: what is left is the shape a
 * trader reads, on an even x grid so a smooth curve through it cannot wobble.
 * The newest sample is always kept exactly, so the line ends where the live
 * marker sits.
 */
export function resampleMean(
  points: readonly ChartPoint[],
  buckets: number
): readonly ChartPoint[] {
  const first = points[0]
  const last = points.at(-1)

  if (!first || !last || buckets < 3 || points.length <= buckets) {
    return points
  }

  const span = last.time - first.time

  if (span <= 0) {
    return points
  }

  const width = span / buckets
  const result: ChartPoint[] = []
  let bucketIndex = 0
  let timeSum = 0
  let valueSum = 0
  let count = 0

  const flush = () => {
    if (count > 0) {
      result.push({ time: Math.round(timeSum / count), value: valueSum / count })
    }

    timeSum = 0
    valueSum = 0
    count = 0
  }

  for (const point of points) {
    const index = Math.min(buckets - 1, Math.floor((point.time - first.time) / width))

    if (index !== bucketIndex) {
      flush()
      bucketIndex = index
    }

    timeSum += point.time
    valueSum += point.value
    count += 1
  }

  flush()

  const tail = result.at(-1)

  if (tail && tail.time !== last.time) {
    result[result.length - 1] = { time: last.time, value: last.value }
  }

  return result
}

/**
 * One [1,2,1] pass over the samples, endpoints untouched. Applied after
 * resampling, it settles the remaining sample-to-sample chatter so the curve
 * reads as one line; the first and last samples stay exact, so the live tip
 * still sits on its real value.
 */
export function smoothPoints(points: readonly ChartPoint[], passes = 1): readonly ChartPoint[] {
  if (points.length < 5 || passes < 1) {
    return points
  }

  let current = points

  for (let pass = 0; pass < passes; pass += 1) {
    const next: ChartPoint[] = [current[0] as ChartPoint]

    for (let index = 1; index < current.length - 1; index += 1) {
      const before = current[index - 1]
      const point = current[index]
      const after = current[index + 1]

      if (!before || !point || !after) {
        continue
      }

      next.push({ time: point.time, value: (before.value + 2 * point.value + after.value) / 4 })
    }

    next.push(current.at(-1) as ChartPoint)
    current = next
  }

  return current
}
