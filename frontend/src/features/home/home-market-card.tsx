import type { ReactNode } from 'react'

import { MarketCardChart, type MarketCardChartLine } from './market-card-chart'
import { MarketCard } from './market-card'
import type { HomeMarketCardModel } from './types'

export interface HomeMarketCardProps {
  card: HomeMarketCardModel
  /** A live status for the footer (a round's countdown) in place of the card's own. */
  status?: ReactNode
}

export function HomeMarketCard({ card, status }: HomeMarketCardProps) {
  const series: readonly MarketCardChartLine[] = card.priceChart
    ? [
        {
          colorToken: '--asset-btc',
          id: 'price',
          points: card.priceChart.points,
        },
      ]
    : (card.series ?? []).map(lineSeries => ({
        colorToken: lineSeries.side === 'away' ? '--market-away' : '--market-home',
        id: lineSeries.side,
        points: lineSeries.points,
      }))

  return (
    <MarketCard
      category={card.category}
      chart={<MarketCardChart label={`${card.title} trend`} series={series} />}
      marketStatus={status ?? card.marketStatus}
      outcomes={card.outcomes}
      slug={card.slug}
      title={card.title}
    />
  )
}
