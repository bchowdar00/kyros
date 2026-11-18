'use client'

import { formatSignedValue, formatValue } from '../lib/format'
import type { Candle } from '../lib/types'
import { useChartContext } from './chart-context'
import { useHoverState } from './hover-store'

/** Open/high/low/close readout for the hovered (or latest) candle. */
export function CandleReadout() {
  const { geometry, hover } = useChartContext()
  const state = useHoverState(hover)
  const { candles, locale, plot, valueFormat } = geometry
  const candle: Candle | undefined = state ? candles[state.index] : candles.at(-1)

  if (!candle) {
    return null
  }

  const change = candle.close - candle.open
  const changeClass = change >= 0 ? 'fill-positive' : 'fill-negative'
  const entries: [string, string][] = [
    ['O', formatValue(candle.open, geometry.axisFormat, locale)],
    ['H', formatValue(candle.high, geometry.axisFormat, locale)],
    ['L', formatValue(candle.low, geometry.axisFormat, locale)],
    ['C', formatValue(candle.close, geometry.axisFormat, locale)],
  ]

  return (
    <text
      className="text-caption font-medium font-mono"
      data-chart-candle-readout=""
      pointerEvents="none"
      x={plot.left + 4}
      y={plot.top + 12}
    >
      {entries.map(([key, value]) => (
        <tspan key={key}>
          <tspan className="fill-muted-foreground">{key} </tspan>
          <tspan className="fill-foreground">{value}</tspan>
          <tspan className="fill-muted-foreground"> </tspan>
        </tspan>
      ))}
      <tspan className={changeClass}>
        {formatSignedValue(change, valueFormat, locale)} (
        {formatSignedValue(candle.open === 0 ? 0 : (change / candle.open) * 100, {
          maximumFractionDigits: 2,
          style: 'percent',
        })}
        )
      </tspan>
    </text>
  )
}
