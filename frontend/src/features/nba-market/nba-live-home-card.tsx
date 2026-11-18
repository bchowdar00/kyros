'use client'

import { env } from '@/env'
import { HomeMarketCard } from '@/features/home/home-market-card'
import type { HomeMarketCardModel } from '@/features/home/types'

import { sideColors } from './teams'
import { useNbaLiveMarket } from './use-nba-live-market'

/**
 * A live NBA card: the home side's market price as the chart and the game's period and
 * clock as the status, from the same signals socket as the market page.
 */
export function NbaLiveHomeMarketCard({ card }: { card: HomeMarketCardModel }) {
  const apiUrl = env.NEXT_PUBLIC_API_URL
  const style = sideColors(
    card.outcomes.find(outcome => outcome.side === 'home')?.label ?? '',
    card.outcomes.find(outcome => outcome.side === 'away')?.label ?? ''
  )
  if (apiUrl === undefined || card.series === undefined) {
    return (
      <div className="contents" style={style}>
        <HomeMarketCard card={card} />
      </div>
    )
  }
  return (
    <div className="contents" style={style}>
      <ConnectedCard apiUrl={apiUrl} card={card} />
    </div>
  )
}

function ConnectedCard({ apiUrl, card }: { apiUrl: string; card: HomeMarketCardModel }) {
  const [home, away] = [
    card.outcomes.find(outcome => outcome.side === 'home')?.label ?? '',
    card.outcomes.find(outcome => outcome.side === 'away')?.label ?? '',
  ]
  const live = useNbaLiveMarket(apiUrl, card.slug, { home, away }, card.range)
  const [homeLine, awayLine] = card.series ?? [
    { label: home, points: [], side: 'home' as const },
    { label: away, points: [], side: 'away' as const },
  ]

  return (
    <HomeMarketCard
      card={{
        ...card,
        marketStatus: live.marketStatus,
        series: [
          { ...homeLine, points: live.pricePoints },
          {
            ...awayLine,
            points: live.pricePoints.map(point => ({
              time: point.time,
              value: Number((100 - point.value).toFixed(2)),
            })),
          },
        ],
      }}
    />
  )
}
