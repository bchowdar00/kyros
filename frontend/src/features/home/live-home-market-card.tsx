'use client'

import { env } from '@/env'
import { usePriceStream } from '@/features/exchange-streams/use-price-stream'

import { HomeMarketCard } from './home-market-card'
import type { HomeMarketCardModel } from './types'

/**
 * A live series card draws the round's ticks from the same `/ws/price` stream as its
 * market page, so the sparkline and the strike are the exchange's, never a placeholder.
 */
export function LiveHomeMarketCard({ card }: { card: HomeMarketCardModel }) {
  return <LivePriceCard card={card} />
}

function LivePriceCard({ card }: { card: HomeMarketCardModel }) {
  const streamed = card.live && card.priceChart !== undefined
  const stream = usePriceStream(
    streamed ? env.NEXT_PUBLIC_API_URL : undefined,
    streamed ? card.slug : undefined
  )

  if (!streamed || card.priceChart === undefined || stream.points.length === 0) {
    return <HomeMarketCard card={card} />
  }

  return (
    <HomeMarketCard
      card={{
        ...card,
        priceChart: {
          ...card.priceChart,
          points: stream.points,
          targetPrice: stream.round?.strike ?? card.priceChart.targetPrice,
        },
        ...(stream.round === undefined
          ? {}
          : { range: { start: stream.round.opensAt, end: stream.round.closesAt } }),
      }}
    />
  )
}
