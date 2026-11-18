import { cents, createMoneyFormatter } from '@tiny/money'
import type { ChartPoint } from '@tiny/ui/charts/lib/types'

import { TimeSeries } from '../widgets/time-series'

export interface PriceVsStrikeProps {
  /** Spot in dollars; the chart lib plots floats and only the labels touch money. */
  points: readonly ChartPoint[]
  strike: number
  roundOpenedAt: number
  timeZone: string
}

const money = createMoneyFormatter('en-US')

/** Spot against the strike, the band shaded on whichever side is currently winning. */
export function PriceVsStrike({ points, strike, roundOpenedAt, timeZone }: PriceVsStrikeProps) {
  const last = points.at(-1)?.value ?? strike
  const above = last >= strike
  const distance = Math.round((last - strike) * 100)
  return (
    <TimeSeries
      label="Bitcoin price against the strike"
      title="BTC vs strike"
      meta={
        <span className={above ? 'font-mono text-positive' : 'font-mono text-negative'}>
          {money.usd(cents(distance), { signDisplay: 'always' })}
        </span>
      }
      series={[{ id: 'spot', label: 'Spot', color: 'var(--asset-btc)', points }]}
      format={{ style: 'currency', currency: 'USD', maximumFractionDigits: 0 }}
      timeZone={timeZone}
      guides={[{ value: strike, label: 'Strike' }]}
      band={{
        kind: 'threshold',
        seriesId: 'spot',
        value: strike,
        side: above ? 'above' : 'below',
        color: above ? 'var(--positive)' : 'var(--negative)',
      }}
      markers={[{ time: roundOpenedAt, label: 'Open' }]}
    />
  )
}
