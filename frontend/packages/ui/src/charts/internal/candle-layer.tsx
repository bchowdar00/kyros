'use client'

import { isBullish } from '../lib/candles'
import { useChartGeometry } from './chart-context'

/** OHLC candles; the bucket width comes from the chart's candle interval. */
export function CandleSeriesLayer() {
  const { candles, candleWidth, plot, xScale, yScale } = useChartGeometry()
  const bodyWidth = Math.max(2, Math.min(candleWidth * 0.62, 18))

  return (
    <g data-chart-candles="">
      {candles.map(candle => {
        const center = xScale(candle.time) + candleWidth / 2

        if (center < plot.left - candleWidth || center > plot.right + candleWidth) {
          return null
        }

        const bullish = isBullish(candle)
        const openY = yScale(candle.open)
        const closeY = yScale(candle.close)
        const top = Math.min(openY, closeY)
        const bodyHeight = Math.max(1, Math.abs(openY - closeY))
        const toneClass = bullish
          ? 'fill-positive stroke-positive'
          : 'fill-negative stroke-negative'

        return (
          <g
            className={toneClass}
            data-chart-candle={candle.time}
            data-direction={bullish ? 'up' : 'down'}
            key={candle.time}
          >
            <line
              strokeWidth={1}
              x1={center}
              x2={center}
              y1={yScale(candle.high)}
              y2={yScale(candle.low)}
            />
            <rect
              height={bodyHeight}
              rx={bodyWidth > 4 ? 1 : 0}
              stroke="none"
              width={bodyWidth}
              x={center - bodyWidth / 2}
              y={top}
            />
          </g>
        )
      })}
    </g>
  )
}
