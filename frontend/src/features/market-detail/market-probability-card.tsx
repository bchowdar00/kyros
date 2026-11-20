'use client'

import { useBrowserTimeZone } from '@kyros/ui/lib/use-browser-time-zone'
import { ProbabilityMarketCard } from '@kyros/ui/charts/probability-market-card'

import { env } from '@/env'
import { useBookStream } from '@/features/exchange-streams/use-book-stream'

import { useTradeMarks, useVenueLines } from './chart-extras'
import type { MarketProbabilityChartData, MarketSide } from './types'

export type MarketProbabilityCardProps = MarketProbabilityChartData

/** A game runs for hours; the other venues' lines are held on the page this long. */
const VENUE_HISTORY_MS = 3 * 60 * 60_000

const seriesColors: Record<MarketSide, string> = {
  away: 'var(--market-away)',
  home: 'var(--market-home)',
}

/**
 * The market screen's outcome chart: the shared probability card, on the hero plot height.
 * On an exchange-backed NBA page it plots the home team's price from `/ws/book`; the book's
 * midpoint line is already a percentage (0–100), the scale the card reads, so no conversion
 * is needed. Without `NEXT_PUBLIC_API_URL` (or a series slug) it renders the supplied points.
 */
export function MarketProbabilityCard({
  live,
  range,
  series,
  seriesSlug,
  timeZone,
}: MarketProbabilityCardProps) {
  const browserTimeZone = useBrowserTimeZone(timeZone)
  const [primary, complement] = series
  const book = useBookStream(env.NEXT_PUBLIC_API_URL, seriesSlug)
  const streaming = seriesSlug !== undefined && env.NEXT_PUBLIC_API_URL !== undefined
  const primaryPoints = streaming ? book.midPoints : primary.points
  const tradeMarks = useTradeMarks()
  const venueLines = useVenueLines(env.NEXT_PUBLIC_API_URL, seriesSlug, VENUE_HISTORY_MS)

  return (
    <ProbabilityMarketCard
      chartClassName="h-chart-lg"
      outcomes={[
        {
          color: seriesColors[primary.side],
          id: primary.side,
          label: primary.label,
          points: primaryPoints,
        },
        { color: seriesColors[complement.side], id: complement.side, label: complement.label },
      ]}
      range={range}
      live={live}
      timeZone={browserTimeZone}
      tradeMarks={tradeMarks}
      venueLines={venueLines}
    />
  )
}
