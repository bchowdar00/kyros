import { formatValue } from '../lib/format'
import { findNearestIndex } from '../lib/scale'
import { formatTooltipTime } from '../lib/time'
import type { ChartHover, ChartHoverValue } from '../lib/types'
import type { ChartGeometry } from './chart-context'
import type { HoverState } from './hover-store'

interface Sample {
  time: number
}

/** The samples the crosshair snaps to: candle centres, or the primary series. */
export function getPrimarySamples(geometry: ChartGeometry): readonly Sample[] {
  if (geometry.kind === 'candlestick') {
    return geometry.candles.map(candle => ({ time: candle.time + geometry.candleInterval / 2 }))
  }

  return geometry.series[0]?.points ?? []
}

export function buildHover(geometry: ChartGeometry, state: HoverState): ChartHover {
  const values: ChartHoverValue[] = []

  for (const item of geometry.series) {
    const index = findNearestIndex(item.points, state.time)
    const point = item.points[index]

    if (point) {
      values.push({ color: item.color, label: item.label, point, seriesId: item.id })
    }
  }

  const hover: ChartHover = { time: state.time, values }

  if (geometry.kind === 'candlestick' && geometry.candles.length > 0) {
    const centers = geometry.candles.map(candle => ({
      time: candle.time + geometry.candleInterval / 2,
    }))
    const candle = geometry.candles[findNearestIndex(centers, state.time)]

    if (candle) {
      hover.candle = candle
    }
  }

  return hover
}

export function describeHover(geometry: ChartGeometry, hover: ChartHover) {
  const time = formatTooltipTime(hover.time, {
    locale: geometry.locale,
    showDate: false,
    timeZone: geometry.timeZone,
  })
  const parts = hover.candle
    ? [
        `open ${formatValue(hover.candle.open, geometry.valueFormat, geometry.locale)}`,
        `high ${formatValue(hover.candle.high, geometry.valueFormat, geometry.locale)}`,
        `low ${formatValue(hover.candle.low, geometry.valueFormat, geometry.locale)}`,
        `close ${formatValue(hover.candle.close, geometry.valueFormat, geometry.locale)}`,
      ]
    : hover.values.map(
        value =>
          `${value.label} ${formatValue(value.point.value, geometry.valueFormat, geometry.locale)}`
      )

  return `${time}, ${parts.join(', ')}`
}
