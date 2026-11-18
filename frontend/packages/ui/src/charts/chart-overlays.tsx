'use client'

import { useChartGeometry } from './internal/chart-context'

export { useChartGeometry }
export type { ChartGeometry, PlotArea } from './internal/chart-context'

export interface ReferenceLineProps {
  dashed?: boolean
  /** Axis label anchored to the target value; live and hover flags render above it. */
  label?: string
  value: number
}

/** Horizontal guide behind the price path; its label never follows live price movement. */
export function ReferenceLine({ dashed = false, label, value }: ReferenceLineProps) {
  const geometry = useChartGeometry()
  const { plot, width, yScale } = geometry
  const y = yScale(value)

  if (!Number.isFinite(y) || y < plot.top - 1 || y > plot.bottom + 1) return null

  return (
    <g data-chart-reference-line={value} pointerEvents="none">
      <line
        className="stroke-muted-foreground"
        opacity={0.8}
        shapeRendering="crispEdges"
        strokeDasharray={dashed ? '5 4' : undefined}
        strokeWidth={1}
        x1={plot.left}
        x2={plot.right}
        y1={y}
        y2={y}
      />
      {label ? (
        <g>
          <text
            className="fill-foreground text-caption font-medium tracking-label uppercase"
            dominantBaseline="middle"
            textAnchor="end"
            x={width}
            y={y}
          >
            {label}
          </text>
        </g>
      ) : null}
    </g>
  )
}

export interface TimeMarkerProps {
  label?: string
  time: number
}

/** Vertical guide at a moment in time, such as tip-off or a market close. */
export function TimeMarker({ label, time }: TimeMarkerProps) {
  const { plot, xScale } = useChartGeometry()
  const x = xScale(time)

  if (x < plot.left - 1 || x > plot.right + 1) {
    return null
  }

  return (
    <g data-chart-time-marker={time} pointerEvents="none">
      <line
        className="stroke-border"
        shapeRendering="crispEdges"
        strokeDasharray="3 3"
        strokeWidth={1}
        x1={x}
        x2={x}
        y1={plot.top}
        y2={plot.bottom}
      />
      {label ? (
        <text
          className="fill-muted-foreground text-caption font-medium tracking-label uppercase"
          dominantBaseline="hanging"
          textAnchor={x > plot.right - 80 ? 'end' : 'start'}
          x={x > plot.right - 80 ? x - 6 : x + 6}
          y={plot.top + 2}
        >
          {label}
        </text>
      ) : null}
    </g>
  )
}
