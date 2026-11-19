import { MINUTE } from '@kyros/ui/charts/lib/time'
import type { ChartPoint, ValueFormat } from '@kyros/ui/charts/lib/types'

import { TimeSeries, type TimeSeriesMarker } from '../widgets/time-series'

export interface CompareSignal {
  id: string
  label: string
  points: readonly ChartPoint[]
}

export interface SignalCompareProps {
  title: string
  /** In fixed order; the first is drawn in the foreground colour, the second in the action colour. */
  signals: readonly [CompareSignal, CompareSignal, ...CompareSignal[]]
  format: ValueFormat
  timeZone: string
  /** Moments to call out, such as a cross. */
  markers?: readonly TimeSeriesMarker[]
}

/** Fixed slot colours: identity follows the entity, never its rank. */
const SLOT_COLORS = ['var(--foreground)', 'var(--action)', 'var(--warning)'] as const

const WINDOWS = [
  { label: '4m', ms: null },
  { label: '2m', ms: 2 * MINUTE },
]

/** Two or more signals on one axis. New even when both are already charted alone. */
export function SignalCompare({ title, signals, format, timeZone, markers }: SignalCompareProps) {
  return (
    <TimeSeries
      label={`${title} comparison`}
      title={title}
      series={signals.map((signal, index) => ({
        id: signal.id,
        label: signal.label,
        color: SLOT_COLORS[index % SLOT_COLORS.length] ?? SLOT_COLORS[0],
        points: signal.points,
      }))}
      format={format}
      timeZone={timeZone}
      windows={WINDOWS}
      {...(markers === undefined ? {} : { markers })}
    />
  )
}
