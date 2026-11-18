'use client'

import { createLinearScale, niceDomain } from '@tiny/ui/charts/lib/scale'
import type { ReactNode } from 'react'

import { ChartShell } from './chart-shell'
import { PLOT_TIME_BASELINE, plotArea } from './plot'

export interface ScatterPoint {
  id: string
  x: number
  y: number
  /** Optional third measure, drawn as marker area. */
  size?: number
}

export interface ScatterProps {
  label: string
  title?: string
  meta?: ReactNode
  points: readonly ScatterPoint[]
  xLabel: string
  yLabel: string
  formatX: (value: number) => string
  formatY: (value: number) => string
  /** One point to call out by id. */
  highlight?: { id: string; label: string }
}

const MIN_RADIUS = 4
const MAX_RADIUS = 8

function ScatterPlot({
  width,
  height,
  points,
  formatX,
  formatY,
  highlight,
}: Pick<ScatterProps, 'points' | 'formatX' | 'formatY'> & {
  width: number
  height: number
  highlight: ScatterProps['highlight']
}) {
  const area = plotArea(width, height)
  const xs = points.map(point => point.x)
  const ys = points.map(point => point.y)
  const xNice = niceDomain(Math.min(...xs), Math.max(...xs), 4)
  const yNice = niceDomain(Math.min(...ys), Math.max(...ys), 3)
  const xScale = createLinearScale(xNice.domain, [area.left + 8, area.right - 8])
  const yScale = createLinearScale(yNice.domain, [area.bottom - 8, area.top + 8])
  const sizes = points.map(point => point.size ?? 1)
  const sizeScale = createLinearScale(
    [Math.min(...sizes), Math.max(...sizes)],
    [MIN_RADIUS, MAX_RADIUS]
  )
  const highlighted = highlight ? points.find(point => point.id === highlight.id) : undefined

  return (
    <svg aria-hidden="true" width={width} height={height} className="block overflow-clip">
      {yNice.ticks.map(tick => (
        <g key={tick}>
          <line
            className={tick === 0 ? 'stroke-border' : 'stroke-border-subtle'}
            x1={area.left}
            x2={area.right}
            y1={yScale(tick)}
            y2={yScale(tick)}
          />
          <text
            className="fill-muted-foreground font-mono text-caption"
            x={width}
            y={yScale(tick)}
            textAnchor="end"
            dominantBaseline="middle"
          >
            {formatY(tick)}
          </text>
        </g>
      ))}
      {xNice.ticks.map(tick => (
        <g key={tick}>
          <line
            className={tick === 0 ? 'stroke-border' : 'stroke-border-subtle'}
            x1={xScale(tick)}
            x2={xScale(tick)}
            y1={area.top}
            y2={area.bottom}
          />
          <text
            className="fill-muted-foreground font-mono text-caption"
            x={xScale(tick)}
            y={height - PLOT_TIME_BASELINE}
            textAnchor="middle"
          >
            {formatX(tick)}
          </text>
        </g>
      ))}
      {points.map(point => (
        <circle
          key={point.id}
          cx={xScale(point.x)}
          cy={yScale(point.y)}
          r={sizeScale(point.size ?? 1)}
          className={
            point.id === highlighted?.id
              ? 'fill-action stroke-background'
              : 'fill-foreground stroke-background'
          }
          fillOpacity={point.id === highlighted?.id ? 1 : 0.55}
          strokeWidth={2}
        />
      ))}
      {highlighted && highlight ? (
        <text
          className="fill-foreground text-caption"
          x={xScale(highlighted.x)}
          y={yScale(highlighted.y) - sizeScale(highlighted.size ?? 1) - 4}
          textAnchor="middle"
        >
          {highlight.label}
        </text>
      ) : null}
    </svg>
  )
}

export function Scatter({
  label,
  title,
  meta,
  points,
  xLabel,
  yLabel,
  formatX,
  formatY,
  highlight,
}: ScatterProps) {
  return (
    <ChartShell
      label={label}
      {...(title === undefined ? {} : { title })}
      value={meta}
      footer={
        <>
          <span>{xLabel} →</span>
          <span>↑ {yLabel}</span>
        </>
      }
    >
      {size => (
        <ScatterPlot
          width={size.width}
          height={size.height}
          points={points}
          formatX={formatX}
          formatY={formatY}
          highlight={highlight}
        />
      )}
    </ChartShell>
  )
}
