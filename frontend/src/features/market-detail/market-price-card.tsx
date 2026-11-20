'use client'

import { useBrowserTimeZone } from '@kyros/ui/lib/use-browser-time-zone'
import { MarketPriceSummary } from './market-price-summary'
import { MarketCountdown } from './market-countdown'
import { PriceMarketCard } from '@kyros/ui/charts/price-market-card'

import { env } from '@/env'

import { useTradeMarks, useVenueLines } from './chart-extras'
import { mergePriceHistory } from './price-history.mapper'

import type { MarketPriceChartData } from './types'
import { useBookStream } from '@/features/exchange-streams/use-book-stream'
import { usePriceStream } from '@/features/exchange-streams/use-price-stream'

export type MarketPriceCardProps = MarketPriceChartData

/**
 * The market screen's strike chart: the shared price card, on the hero plot height. The
 * exchange stream is authoritative for the round: its strike and close time replace the
 * catalog's once the snapshot arrives. The probability view plots the book's own midpoint
 * from `/ws/book`, never a mapping of the BTC price.
 */
export function MarketPriceCard({
  label,
  points,
  priceBand,
  priceSymbol,
  recentMarketResults,
  range,
  targetLabel,
  targetPrice,
  timeZone,
  windows,
}: MarketPriceCardProps) {
  const browserTimeZone = useBrowserTimeZone(timeZone)
  const stream = usePriceStream(env.NEXT_PUBLIC_API_URL, priceSymbol)
  const book = useBookStream(env.NEXT_PUBLIC_API_URL, priceSymbol)
  const usingLiveFeed = env.NEXT_PUBLIC_API_URL !== undefined && priceSymbol !== undefined
  const chartPoints = mergePriceHistory(points, stream.points)
  const strike = stream.round?.strike ?? targetPrice
  const closesAt = stream.round?.closesAt ?? range.end
  const tradeMarks = useTradeMarks()
  const venueLines = useVenueLines(env.NEXT_PUBLIC_API_URL, priceSymbol)

  return (
    <div className="flex flex-col gap-lg">
      <MarketPriceSummary
        feed={usingLiveFeed ? stream.feed : { state: 'paused' }}
        price={chartPoints.at(-1)?.value ?? Number.NaN}
        targetLabel={targetLabel}
        targetPrice={strike}
        timeLeft={<MarketCountdown endsAt={closesAt} />}
      />
      <PriceMarketCard
        asset={{ color: 'var(--asset-btc)', symbol: label }}
        chartClassName="h-chart-lg"
        feed={usingLiveFeed ? stream.feed : undefined}
        points={chartPoints}
        priceBand={priceBand}
        probabilityPoints={usingLiveFeed ? book.midPoints : undefined}
        recentMarketResults={recentMarketResults}
        round={stream.round}
        targetLabel={targetLabel}
        targetPrice={strike}
        timeZone={browserTimeZone}
        tradeMarks={tradeMarks}
        venueLines={venueLines}
        windows={windows}
      />
    </div>
  )
}
