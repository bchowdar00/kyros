'use client'

import { env } from '@/env'
import { useBookStream } from '@/features/exchange-streams/use-book-stream'
import { usePriceStream } from '@/features/exchange-streams/use-price-stream'

import { HomeMarketCard } from './home-market-card'
import type { HomeMarketCardModel } from './types'

/**
 * A live series card draws from the same stream as its market page (`/ws/price` for a strike
 * market, `/ws/book` for a game), so the sparkline is the exchange's, never a placeholder.
 */
export function LiveHomeMarketCard({ card }: { card: HomeMarketCardModel }) {
  return card.priceChart === undefined ? (
    <LiveOutcomeCard card={card} />
  ) : (
    <LivePriceCard card={card} />
  )
}

const FULL_PERCENT = 100

/**
 * A live card with no price underneath it (a game) draws what the market thinks: the first
 * outcome's price from `/ws/book`, a percentage, and its complement for the other outcome.
 * Until the book prints a midpoint the card stays as the server rendered it.
 */
function LiveOutcomeCard({ card }: { card: HomeMarketCardModel }) {
  const streamed = card.live && card.series !== undefined
  const book = useBookStream(
    streamed ? env.NEXT_PUBLIC_API_URL : undefined,
    streamed ? card.slug : undefined
  )

  if (card.series === undefined || book.midPoints.length === 0) {
    return <HomeMarketCard card={card} />
  }

  const [quoted, other] = card.series

  return (
    <HomeMarketCard
      card={{
        ...card,
        series: [
          { ...quoted, points: book.midPoints },
          {
            ...other,
            points: book.midPoints.map(point => ({
              time: point.time,
              value: FULL_PERCENT - point.value,
            })),
          },
        ],
      }}
    />
  )
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
