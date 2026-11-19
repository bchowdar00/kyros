import type { EventDetail, MarketRecord } from '@kyros/domain'
import { CANDLE_INTERVALS } from '@kyros/ui/charts/lib/candles'
import { MINUTE, SECOND } from '@kyros/ui/charts/lib/time'
import type { TimeRange } from '@kyros/ui/charts/lib/types'

import { decimalToNumber } from './price-tick'
import type { MarketDetail, MarketPriceChartData } from './types'

const STRIKE_CHART_BAND = 300

function firstAutomatedMarket(detail: EventDetail): MarketRecord | undefined {
  return detail.markets.find(market => market.metadata.automation !== undefined)
}

export function windowRange(detail: EventDetail): { end: number; start: number } | undefined {
  const start = Date.parse(detail.startsAt)
  const end = Date.parse(detail.endsAt)

  if (!Number.isFinite(start) || !Number.isFinite(end) || end <= start) {
    return undefined
  }

  return { end, start }
}

function toPriceChart(detail: EventDetail, market: MarketRecord): MarketPriceChartData | undefined {
  const automation = market.metadata.automation
  const range = windowRange(detail)

  if (automation === undefined || range === undefined) {
    return undefined
  }

  const reference = decimalToNumber(automation.reference.mantissa, automation.reference.exponent)

  if (reference === undefined) {
    return undefined
  }

  // The exchange catalog carries no strike (see exchange-catalog.mapper.ts). A zero
  // reference means unknown: the card shows it as such until the price stream's snapshot
  // supplies the round's strike. Nothing is ever charted against a fabricated target.
  const targetPrice = reference === 0 ? Number.NaN : reference

  const durationMs = range.end - range.start

  return {
    label: 'BTC',
    live: true,
    points: [],
    priceBand: STRIKE_CHART_BAND,
    // For the Rust exchange this is the series slug the `/ws/price` stream is keyed on.
    priceSymbol: automation.priceSymbol,
    range,
    targetLabel: 'Price to beat',
    targetPrice,
    timeZone: 'UTC',
    windows: [
      {
        candleInterval: durationMs <= MINUTE ? 5 * SECOND : CANDLE_INTERVALS['1m'],
        durationMs,
      },
    ],
  }
}

export function resolvePriceMarket(detail: EventDetail) {
  const market = firstAutomatedMarket(detail)
  const priceChart = market === undefined ? undefined : toPriceChart(detail, market)

  return market === undefined || priceChart === undefined ? undefined : { market, priceChart }
}

/**
 * The automated market the event rolls over on, once its window can be plotted. Unlike
 * `resolvePriceMarket` this does not require a price chart, so an NBA round (which charts
 * probability, not price) resolves too.
 */
export function resolveActiveMarket(detail: EventDetail): MarketRecord | undefined {
  const market = firstAutomatedMarket(detail)

  return market !== undefined && windowRange(detail) !== undefined ? market : undefined
}

export function toActiveMarketIdentity(detail: EventDetail): { eventId: number } | undefined {
  return resolveActiveMarket(detail) === undefined ? undefined : { eventId: detail.id }
}

/**
 * The window the market header counts down to. A game ends at its final, not at a time on
 * the clock, so a market with a scoreboard has none.
 */
export function headerCountdownRange(
  market: Pick<MarketDetail, 'priceChart' | 'probabilityChart' | 'scoreboard'>
): TimeRange | undefined {
  if (market.scoreboard !== undefined) return undefined
  return market.priceChart?.range ?? market.probabilityChart?.range
}
