'use client'

import { estimateTextWidth, formatValue } from '../lib/format'
import { formatTimeTick } from '../lib/time'
import { type ChartGeometry, useChartGeometry } from './chart-context'

export type GridTone = 'strong' | 'subtle'

const gridStrokeClass: Record<GridTone, string> = {
  strong: 'stroke-highlight',
  subtle: 'stroke-border-subtle',
}

export interface ChartGridProps {
  tone?: GridTone
}

export function ChartGrid({ tone = 'subtle' }: ChartGridProps) {
  const { plot, yScale, yTicks } = useChartGeometry()

  return (
    <g aria-hidden="true" data-chart-grid="">
      {yTicks.map(tick => {
        const y = yScale(tick)

        if (y < plot.top - 1 || y > plot.bottom + 1) {
          return null
        }

        return (
          <line
            className={gridStrokeClass[tone]}
            key={tick}
            shapeRendering="crispEdges"
            strokeWidth={1}
            x1={plot.left}
            x2={plot.right}
            y1={y}
            y2={y}
          />
        )
      })}
    </g>
  )
}

/** Vertical clearance kept around a value flag, so its label is the only one there. */
const TICK_CLEARANCE_PX = 14

export interface ChartYAxisProps {
  /** Values whose neighbouring tick labels are dropped, because overlays mark them instead. */
  hideNear?: number | readonly number[] | undefined
}

export function ChartYAxis({ hideNear }: ChartYAxisProps) {
  const { axisFontSize, axisFormat, locale, plot, width, yScale, yTicks } = useChartGeometry()
  const fontClass = axisFontSize > 10 ? 'text-label' : 'text-caption'
  const hidden = hideNear === undefined ? [] : [hideNear].flat()
  const hiddenYs = hidden.filter(value => Number.isFinite(value)).map(value => yScale(value))

  return (
    <g aria-hidden="true" data-chart-y-axis="">
      {yTicks.map(tick => {
        const y = yScale(tick)

        if (y < plot.top - 1 || y > plot.bottom + 1) {
          return null
        }

        if (hiddenYs.some(hiddenY => Math.abs(y - hiddenY) < TICK_CLEARANCE_PX)) {
          return null
        }

        return (
          <text
            className={`fill-muted-foreground font-medium font-mono ${fontClass}`}
            dominantBaseline="middle"
            key={tick}
            textAnchor="end"
            x={width}
            y={y}
          >
            {formatValue(tick, axisFormat, locale)}
          </text>
        )
      })}
    </g>
  )
}

interface TimeLabel {
  anchor: 'end' | 'middle' | 'start'
  label: string
  tick: number
  x: number
}

const LABEL_MIN_GAP = 12

/** Anchors edge labels inward and drops any label that would overlap its neighbour. */
function placeTimeLabels(geometry: ChartGeometry): TimeLabel[] {
  const { axisFontSize, locale, plot, range, timeZone, xScale, xTicks } = geometry
  const placed: TimeLabel[] = []
  let lastRight = Number.NEGATIVE_INFINITY

  for (const tick of xTicks.values) {
    const x = xScale(tick)
    const label = formatTimeTick(tick, xTicks.detail, { locale, timeZone })
    const textWidth = estimateTextWidth(label, axisFontSize)
    const anchor = tick <= range.start ? 'start' : tick >= range.end ? 'end' : 'middle'
    const left = anchor === 'start' ? x : anchor === 'end' ? x - textWidth : x - textWidth / 2

    if (
      left < lastRight + LABEL_MIN_GAP ||
      left < plot.left - 1 ||
      left + textWidth > plot.right + 1
    ) {
      continue
    }

    lastRight = left + textWidth
    placed.push({ anchor, label, tick, x })
  }

  return placed
}

/** The placed tick closest to `time`, or none when emphasis is off. */
function nearestPlacedTick(placed: readonly TimeLabel[], time: number | undefined) {
  if (time === undefined) {
    return undefined
  }

  let nearest: number | undefined
  let nearestDistance = Number.POSITIVE_INFINITY

  for (const item of placed) {
    const distance = Math.abs(item.tick - time)

    if (distance < nearestDistance) {
      nearestDistance = distance
      nearest = item.tick
    }
  }

  return nearest
}

export interface ChartXAxisProps {
  /** Time whose nearest label renders in the foreground colour, marking the latest sample. */
  emphasizeTime?: number | undefined
}

export function ChartXAxis({ emphasizeTime }: ChartXAxisProps) {
  const geometry = useChartGeometry()
  const fontClass = geometry.axisFontSize > 10 ? 'text-label' : 'text-caption'
  const placed = placeTimeLabels(geometry)
  const emphasizedTick = nearestPlacedTick(placed, emphasizeTime)

  return (
    <g aria-hidden="true" data-chart-x-axis="">
      {placed.map(item => (
        <text
          className={`${
            item.tick === emphasizedTick ? 'fill-foreground' : 'fill-muted-foreground'
          } font-medium font-mono ${fontClass}`}
          data-chart-now-tick={item.tick === emphasizedTick ? '' : undefined}
          key={item.tick}
          textAnchor={item.anchor}
          x={item.x}
          y={geometry.height - 2}
        >
          {item.label}
        </text>
      ))}
    </g>
  )
}
