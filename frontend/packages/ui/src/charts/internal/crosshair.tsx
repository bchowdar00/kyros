'use client'

import { estimateTextWidth, formatValue } from '../lib/format'
import { findNearestIndex } from '../lib/scale'
import { formatTooltipTime } from '../lib/time'
import type { ChartHover, ChartPoint } from '../lib/types'
import { useChartContext } from './chart-context'
import type { HoverState } from './hover-store'

const TIME_PILL_HEIGHT = 24
const TAG_HEIGHT = 24

/**
 * The drawn line's value at `time`, read between its two neighbouring stroke
 * points. The dot rides the line the reader sees; the readouts keep the observed value.
 */
function strokeValueAt(points: readonly ChartPoint[], time: number): number | undefined {
  const index = findNearestIndex(points, time)
  const nearest = points[index]

  if (!nearest) {
    return undefined
  }

  const other = points[nearest.time <= time ? index + 1 : index - 1]

  if (!other || other.time === nearest.time) {
    return nearest.value
  }

  const weight = (time - nearest.time) / (other.time - nearest.time)

  return nearest.value + (other.value - nearest.value) * Math.min(1, Math.max(0, weight))
}

export function Crosshair({ hover, state }: { hover: ChartHover; state: HoverState }) {
  const { geometry } = useChartContext()
  const {
    axisFontSize,
    density,
    displaySeries,
    glowFilterId,
    kind,
    locale,
    plot,
    timeZone,
    width,
    yScale,
  } = geometry
  // A compact chart has no room under the axis or above the plot; the line and dots
  // mark the cursor and the host reads the hovered value out through `onHoverChange`.
  const showPills = density === 'default'
  const fontClass = axisFontSize > 10 ? 'text-label' : 'text-caption'
  const timeLabel = formatTooltipTime(hover.time, {
    locale,
    showDate: false,
    showSeconds: true,
    timeZone,
  })
  const timeWidth = estimateTextWidth(timeLabel, axisFontSize) + 16
  const tagValue = hover.candle?.close ?? hover.values[0]?.point.value
  const showTag = showPills && kind !== 'probability' && tagValue !== undefined
  const tagLabel = showTag ? formatValue(tagValue, geometry.axisFormat, locale) : ''
  const tagWidth = estimateTextWidth(tagLabel, axisFontSize) + 12
  // Keep both labels on one vertical anchor, including when either meets an edge.
  const halfLabelWidth = Math.max(timeWidth, showTag ? tagWidth : 0) / 2
  const labelX = Math.min(Math.max(state.x, plot.left + halfLabelWidth), width - halfLabelWidth)
  const timeX = labelX - timeWidth / 2
  const tagX = labelX - tagWidth / 2

  return (
    <g data-chart-crosshair="" pointerEvents="none" role="tooltip" aria-label="Historical price">
      <line
        className="stroke-border"
        shapeRendering="crispEdges"
        strokeWidth={1}
        x1={state.x}
        x2={state.x}
        y1={plot.top}
        y2={plot.bottom}
      />

      {kind === 'candlestick'
        ? null
        : hover.values.map(value => {
            const stroke = displaySeries?.find(item => item.id === value.seriesId)?.points
            const y = yScale(
              (stroke ? strokeValueAt(stroke, hover.time) : undefined) ?? value.point.value
            )

            return (
              <g data-chart-hover-dot={value.seriesId} key={value.seriesId}>
                <circle
                  cx={state.x}
                  cy={y}
                  fill={value.color}
                  filter={`url(#${glowFilterId})`}
                  r={5}
                />
                <circle className="fill-background" cx={state.x} cy={y} r={6} />
                <circle cx={state.x} cy={y} fill={value.color} r={4} />
              </g>
            )
          })}

      {showPills ? (
        <g transform={`translate(${timeX}, ${plot.bottom + 5})`}>
          <rect
            className="fill-surface-raised"
            height={TIME_PILL_HEIGHT}
            rx={2}
            width={timeWidth}
          />
          <text
            className={`fill-foreground font-medium font-mono ${fontClass}`}
            dominantBaseline="middle"
            textAnchor="middle"
            x={timeWidth / 2}
            y={TIME_PILL_HEIGHT / 2 + 0.5}
          >
            {timeLabel}
          </text>
        </g>
      ) : null}

      {showTag ? (
        <g data-chart-axis-tag="" transform={`translate(${tagX}, 0)`}>
          <rect className="fill-foreground" height={TAG_HEIGHT} rx={2} width={tagWidth} />
          <text
            className={`fill-background font-medium font-mono ${fontClass}`}
            dominantBaseline="middle"
            textAnchor="middle"
            x={tagWidth / 2}
            y={TAG_HEIGHT / 2 + 0.5}
          >
            {tagLabel}
          </text>
        </g>
      ) : null}
    </g>
  )
}
