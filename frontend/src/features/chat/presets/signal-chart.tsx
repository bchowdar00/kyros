import { MINUTE } from '@kyros/ui/charts/lib/time'
import type { ChartPoint, ValueFormat } from '@kyros/ui/charts/lib/types'

import { TimeSeries, type TimeSeriesGuide } from '../widgets/time-series'

/** Guide lines a signal is read against; the chart draws them whenever the signal has any. */
const SIGNAL_GUIDES: Record<string, readonly TimeSeriesGuide[]> = {
  rsi_14: [
    { value: 30, label: 'Oversold' },
    { value: 70, label: 'Overbought' },
  ],
  bb_pct_b: [{ value: 0 }, { value: 1 }],
  edge: [{ value: 0 }],
  zscore: [{ value: 0 }],
}

/** Fixed domains for bounded signals, so the plot never rescales around noise. */
const SIGNAL_DOMAINS: Record<string, readonly [number, number]> = {
  rsi_14: [0, 100],
  bb_pct_b: [-0.25, 1.25],
}

export interface SignalChartProps {
  signalId: string
  signalLabel: string
  points: readonly ChartPoint[]
  format: ValueFormat
  timeZone: string
}

const WINDOWS = [
  { label: '4m', ms: null },
  { label: '2m', ms: 2 * MINUTE },
  { label: '1m', ms: MINUTE },
]

/** One signal on its own default window with the guides it is read against. */
export function SignalChart({ signalId, signalLabel, points, format, timeZone }: SignalChartProps) {
  const guides = SIGNAL_GUIDES[signalId]
  const domain = SIGNAL_DOMAINS[signalId]
  return (
    <TimeSeries
      label={`${signalLabel} signal chart`}
      title={signalLabel}
      series={[{ id: signalId, label: signalLabel, color: 'var(--foreground)', points }]}
      format={format}
      timeZone={timeZone}
      windows={WINDOWS}
      {...(guides === undefined ? {} : { guides })}
      {...(domain === undefined ? {} : { domain })}
    />
  )
}
