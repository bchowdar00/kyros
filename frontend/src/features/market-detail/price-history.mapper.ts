import type { ChartPoint } from '@kyros/ui/charts/lib/types'
import { decimalToNumber } from './price-tick'

const MAX_HISTORY_POINTS = 4_000

/** Live observations own their entire time interval; sampled history fills only outside it. */
export function mergePriceHistory(
  history: readonly ChartPoint[],
  ticks: readonly ChartPoint[]
): readonly ChartPoint[] {
  const firstTick = ticks[0]
  const lastTick = ticks.at(-1)
  if (!firstTick || !lastTick) return history
  // A refreshed history grid uses different timestamps. Interleaving it with raw
  // ticks can insert older carried prices between newer observations at rollover.
  const outsideLive = history.filter(
    point => point.time < firstTick.time || point.time > lastTick.time
  )
  const byTime = new Map(outsideLive.map(point => [point.time, point]))
  for (const point of ticks) byTime.set(point.time, point)
  return [...byTime.values()]
    .toSorted((left, right) => left.time - right.time)
    .slice(-MAX_HISTORY_POINTS)
}

interface PriceHistoryResponse {
  points: readonly { t: number; mantissa: string; expo: number }[]
  firstTickAtUs: number | null
  lastTickAtUs: number | null
}

/** Coverage bounds prevent the server's carried-forward grid from inventing trailing history. */
export function toPriceHistory(history: PriceHistoryResponse): readonly ChartPoint[] {
  if (history.firstTickAtUs === null || history.lastTickAtUs === null) return []
  const firstTime = history.firstTickAtUs / 1_000
  const lastTime = history.lastTickAtUs / 1_000
  return history.points.flatMap(point => {
    const time = point.t * 1_000
    const value = decimalToNumber(point.mantissa, point.expo)
    return value === undefined || time < firstTime || time > lastTime ? [] : [{ time, value }]
  })
}
