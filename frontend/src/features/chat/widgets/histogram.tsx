'use client'

import { buildLinePath } from '@tiny/ui/charts/lib/path'
import { createLinearScale } from '@tiny/ui/charts/lib/scale'
import type { ReactNode } from 'react'

import { ChartShell } from './chart-shell'
import { PLOT_TIME_BASELINE, plotArea } from './plot'

export interface HistogramProps {
  label: string
  title?: string
  meta?: ReactNode
  values: readonly number[]
  bins?: number
  format: (value: number) => string
  /** A value to pin on the distribution, such as where the market is right now. */
  current?: { value: number; label: string }
  /** Draw a smoothed density curve over the bins. */
  density?: boolean
}

const DEFAULT_BINS = 24
const BIN_GAP = 2

function binCounts(values: readonly number[], bins: number, min: number, max: number) {
  const counts = Array.from({ length: bins }, () => 0)
  const span = max - min || 1
  for (const value of values) {
    const index = Math.min(bins - 1, Math.floor(((value - min) / span) * bins))
    counts[index] = (counts[index] ?? 0) + 1
  }
  return counts
}

/** Gaussian smoothing over neighbouring bins; a curve that follows the shape, not the noise. */
function smooth(counts: readonly number[], radius = 2) {
  return counts.map((_, index) => {
    let total = 0
    let weight = 0
    for (let offset = -radius; offset <= radius; offset += 1) {
      const neighbour = counts[index + offset]
      if (neighbour === undefined) continue
      const w = Math.exp(-(offset * offset) / (2 * (radius / 2) ** 2))
      total += neighbour * w
      weight += w
    }
    return weight ? total / weight : 0
  })
}

function HistogramPlot({
  width,
  height,
  values,
  bins,
  format,
  current,
  density,
}: Required<Pick<HistogramProps, 'values' | 'bins' | 'format' | 'density'>> & {
  width: number
  height: number
  current: HistogramProps['current']
}) {
  const area = plotArea(width, height, 0)
  const min = Math.min(...values, current?.value ?? Number.POSITIVE_INFINITY)
  const max = Math.max(...values, current?.value ?? Number.NEGATIVE_INFINITY)
  const counts = binCounts(values, bins, min, max)
  const peak = Math.max(1, ...counts)
  const xScale = createLinearScale([min, max], [area.left, area.right])
  const yScale = createLinearScale([0, peak], [area.bottom, area.top])
  const binWidth = (area.right - area.left) / bins
  const currentBin =
    current === undefined
      ? -1
      : Math.min(bins - 1, Math.floor(((current.value - min) / (max - min || 1)) * bins))
  const curve = density
    ? buildLinePath(
        smooth(counts).map((count, index) => ({
          x: area.left + binWidth * (index + 0.5),
          y: yScale(count),
        })),
        'monotone'
      )
    : ''

  return (
    <svg aria-hidden="true" width={width} height={height} className="block overflow-clip">
      <line
        className="stroke-border"
        x1={area.left}
        x2={area.right}
        y1={area.bottom}
        y2={area.bottom}
      />
      {counts.map((count, index) => (
        <rect
          key={index}
          x={area.left + binWidth * index + BIN_GAP / 2}
          y={yScale(count)}
          width={Math.max(1, binWidth - BIN_GAP)}
          height={area.bottom - yScale(count)}
          className={index === currentBin ? 'fill-action' : 'fill-foreground'}
          fillOpacity={index === currentBin ? 0.9 : 0.35}
        />
      ))}
      {density ? (
        <path
          d={curve}
          fill="none"
          className="stroke-foreground"
          strokeWidth={2}
          strokeLinejoin="round"
        />
      ) : null}
      {current ? (
        <g>
          <line
            className="stroke-action"
            x1={xScale(current.value)}
            x2={xScale(current.value)}
            y1={area.top}
            y2={area.bottom}
          />
          <text
            className="fill-foreground text-caption"
            x={xScale(current.value)}
            y={area.top + 8}
            textAnchor={xScale(current.value) > (area.left + area.right) / 2 ? 'end' : 'start'}
            dx={xScale(current.value) > (area.left + area.right) / 2 ? -6 : 6}
          >
            {current.label} <tspan className="font-mono">{format(current.value)}</tspan>
          </text>
        </g>
      ) : null}
      {[min, (min + max) / 2, max].map((tick, index) => (
        <text
          key={tick}
          className="fill-muted-foreground font-mono text-caption"
          x={xScale(tick)}
          y={height - PLOT_TIME_BASELINE}
          textAnchor={index === 0 ? 'start' : index === 2 ? 'end' : 'middle'}
        >
          {format(tick)}
        </text>
      ))}
    </svg>
  )
}

export function Histogram({
  label,
  title,
  meta,
  values,
  bins = DEFAULT_BINS,
  format,
  current,
  density = false,
}: HistogramProps) {
  return (
    <ChartShell label={label} {...(title === undefined ? {} : { title })} value={meta}>
      {size => (
        <HistogramPlot
          width={size.width}
          height={size.height}
          values={values}
          bins={bins}
          format={format}
          current={current}
          density={density}
        />
      )}
    </ChartShell>
  )
}
