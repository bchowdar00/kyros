import { DAY, HOUR, MINUTE } from './time'
import type { Candle, ChartPoint, TimeRange } from './types'

export const CANDLE_INTERVALS = {
  '1m': MINUTE,
  '5m': 5 * MINUTE,
  '15m': 15 * MINUTE,
  '1h': HOUR,
  '4h': 4 * HOUR,
  '1d': DAY,
} as const

export type CandleInterval = keyof typeof CANDLE_INTERVALS

/** Buckets tick samples into OHLC candles of `intervalMs`, skipping empty buckets. */
export function aggregateCandles(
  points: readonly ChartPoint[],
  intervalMs: number,
  range?: TimeRange
): Candle[] {
  if (!(intervalMs > 0)) {
    return []
  }

  const buckets = new Map<number, Candle>()

  for (const point of points) {
    if (range && (point.time < range.start || point.time > range.end)) {
      continue
    }

    const bucketStart = Math.floor(point.time / intervalMs) * intervalMs
    const candle = buckets.get(bucketStart)

    if (!candle) {
      const next: Candle = {
        close: point.value,
        high: point.value,
        low: point.value,
        open: point.value,
        time: bucketStart,
      }

      if (point.volume !== undefined) {
        next.volume = point.volume
      }

      buckets.set(bucketStart, next)
      continue
    }

    candle.close = point.value
    candle.high = Math.max(candle.high, point.value)
    candle.low = Math.min(candle.low, point.value)

    if (point.volume !== undefined) {
      candle.volume = (candle.volume ?? 0) + point.volume
    }
  }

  return [...buckets.values()].toSorted((a, b) => a.time - b.time)
}

export function isBullish(candle: Candle) {
  return candle.close >= candle.open
}
