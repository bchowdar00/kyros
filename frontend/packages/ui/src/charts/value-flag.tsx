'use client'

import { useChartGeometry } from './internal/chart-context'
import { estimateTextWidth, formatValue } from './lib/format'

const FLAG_HEIGHT = 20
const FLAG_FONT_SIZE = 12

export interface ValueFlagProps {
  /** Fill of the flag; the label is drawn in the background colour on top of it. */
  color: string
  /** Rendered instead of the formatted value when given. */
  label?: string
  value: number
  seriesId?: string
}

/**
 * Solid flag in the right-hand axis gutter marking a live value, such as the
 * latest price. It reads as the axis label for where the line currently sits,
 * so pass it through `overlays`, which renders outside the plot clip.
 */
export function ValueFlag({ color, label, value, seriesId }: ValueFlagProps) {
  const { displaySeries, locale, plot, valueFormat, width, yScale } = useChartGeometry()
  const displayValue =
    displaySeries?.find(series => series.id === seriesId)?.points.at(-1)?.value ?? value
  const y = yScale(displayValue)

  if (!Number.isFinite(y) || y < plot.top - 1 || y > plot.bottom + 1) {
    return null
  }

  const text = label ?? formatValue(value, valueFormat, locale)
  const flagWidth = Math.ceil(estimateTextWidth(text, FLAG_FONT_SIZE) + 12)
  const left = width - flagWidth

  return (
    <g data-chart-value-flag={value} pointerEvents="none">
      <g transform={`translate(${left}, ${y - FLAG_HEIGHT / 2})`}>
        <rect fill={color} height={FLAG_HEIGHT} rx={4} width={flagWidth} />
        <text
          className="fill-background text-label font-medium font-mono"
          dominantBaseline="middle"
          textAnchor="middle"
          x={flagWidth / 2}
          y={FLAG_HEIGHT / 2 + 0.5}
        >
          {text}
        </text>
      </g>
    </g>
  )
}
