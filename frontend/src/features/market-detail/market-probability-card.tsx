'use client'

import { useBrowserTimeZone } from '@kyros/ui/lib/use-browser-time-zone'
import { ProbabilityMarketCard } from '@kyros/ui/charts/probability-market-card'

import type { MarketProbabilityChartData, MarketSide } from './types'

export type MarketProbabilityCardProps = MarketProbabilityChartData

const seriesColors: Record<MarketSide, string> = {
  away: 'var(--market-away)',
  home: 'var(--market-home)',
}

/** The market screen's outcome chart: the shared probability card, on the hero plot height. */
export function MarketProbabilityCard({
  live,
  range,
  series,
  timeZone,
}: MarketProbabilityCardProps) {
  const browserTimeZone = useBrowserTimeZone(timeZone)
  const [primary, complement] = series

  return (
    <ProbabilityMarketCard
      chartClassName="h-chart-lg"
      outcomes={[
        {
          color: seriesColors[primary.side],
          id: primary.side,
          label: primary.label,
          points: primary.points,
        },
        { color: seriesColors[complement.side], id: complement.side, label: complement.label },
      ]}
      range={range}
      live={live}
      timeZone={browserTimeZone}
    />
  )
}
