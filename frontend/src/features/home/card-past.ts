import type { ChartPoint } from '@kyros/ui/charts/lib/types'

import type { HomeMarketCardModel } from './types'

/**
 * The past of a game card's line. A game's book can sit still for hours before tipoff, and a
 * card that only drew what moved since the page opened would have nothing to draw; it opens
 * instead with the price the game traded at over the last half day, as its chart does.
 */

const FULL_PERCENT = 100
/** A line needs two points. */
const FEWEST_FOR_A_LINE = 2
/** Two decimals of a percent: finer than any price here is quoted. */
const STEPS = 100

function toPercent(fraction: number): number {
  return Math.round(fraction * FULL_PERCENT * STEPS) / STEPS
}

/**
 * A game card with its two lines opened by the home side's past price, a fraction of a dollar
 * per reading, and what is left of the dollar for the other side. A card that charts a price
 * against a strike, or a past too short to be a line, is returned as it is.
 */
export function withCardPast(
  card: HomeMarketCardModel,
  past: readonly { time: number; value: number }[] | undefined
): HomeMarketCardModel {
  if (card.series === undefined || past === undefined || past.length < FEWEST_FOR_A_LINE) {
    return card
  }
  const home = past.map(reading => ({ time: reading.time, value: toPercent(reading.value) }))
  const away = home.map(point => ({
    time: point.time,
    value: Math.round((FULL_PERCENT - point.value) * STEPS) / STEPS,
  }))
  const [first, second] = card.series
  return {
    ...card,
    series: [
      { ...first, points: first.side === 'home' ? home : away },
      { ...second, points: second.side === 'home' ? home : away },
    ],
  }
}

/** The past that came before the first live point, followed by the live points. */
export function joinCardLine(
  past: readonly ChartPoint[],
  live: readonly ChartPoint[]
): readonly ChartPoint[] {
  const first = live[0]
  if (first === undefined) return past
  return [...past.filter(point => point.time < first.time), ...live]
}
