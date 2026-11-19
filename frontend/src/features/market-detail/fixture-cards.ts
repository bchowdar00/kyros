import type { HomeMarketCardModel } from '@/features/home/types'

import { MARKET_DETAIL_FIXTURES } from './market-detail.fixture'
import type { MarketDetail } from './types'

function toFixtureCard(market: MarketDetail): HomeMarketCardModel | undefined {
  const [away, home] = market.header.outcomes
  const chart = market.priceChart ?? market.probabilityChart

  if (chart === undefined) {
    return undefined
  }

  return {
    category: market.category,
    live: chart.live,
    outcomes: [
      {
        label: away.label,
        side: away.side,
        ...(away.priceCents === undefined ? {} : { priceCents: away.priceCents }),
      },
      {
        label: home.label,
        side: home.side,
        ...(home.priceCents === undefined ? {} : { priceCents: home.priceCents }),
      },
    ],
    ...(market.priceChart === undefined
      ? {}
      : {
          priceChart: {
            label: market.priceChart.label,
            points: market.priceChart.points,
            targetLabel: market.priceChart.targetLabel,
            targetPrice: market.priceChart.targetPrice,
          },
        }),
    range: chart.range,
    ...(market.probabilityChart === undefined
      ? {}
      : {
          series: [
            {
              label: market.probabilityChart.series[0].label,
              points: market.probabilityChart.series[0].points,
              side: market.probabilityChart.series[0].side,
            },
            {
              label: market.probabilityChart.series[1].label,
              points: market.probabilityChart.series[1].points,
              side: market.probabilityChart.series[1].side,
            },
          ],
        }),
    slug: market.slug,
    subtitle: market.header.subtitle,
    timeZone: chart.timeZone,
    title: market.header.title,
  }
}

/** Static home cards for local development with no exchange configured. */
export function fixtureHomeCards(): readonly HomeMarketCardModel[] {
  const cards: HomeMarketCardModel[] = []

  for (const market of Object.values(MARKET_DETAIL_FIXTURES)) {
    const card = toFixtureCard(market)

    if (card !== undefined) {
      cards.push(card)
    }
  }

  return cards
}
