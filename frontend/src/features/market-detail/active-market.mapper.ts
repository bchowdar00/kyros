import type { EventDetail, MarketRecord } from '@kyros/domain'
import { CANDLE_INTERVALS } from '@kyros/ui/charts/lib/candles'
import { MINUTE, SECOND } from '@kyros/ui/charts/lib/time'

import { decimalToNumber } from './price-tick'
import type { MarketPriceChartData } from './types'

const STRIKE_CHART_BAND = 300

function firstAutomatedMarket(detail: EventDetail): MarketRecord | undefined {
  return detail.markets.find(market => market.metadata.automation !== undefined)
}

function windowRange(detail: EventDetail): { end: number; start: number } | undefined {
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

export function toActiveMarketIdentity(detail: EventDetail): { eventId: number } | undefined {
  return resolvePriceMarket(detail) === undefined ? undefined : { eventId: detail.id }
}
