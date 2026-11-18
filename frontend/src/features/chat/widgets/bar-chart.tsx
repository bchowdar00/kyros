'use client'

import { createLinearScale, niceDomain } from '@tiny/ui/charts/lib/scale'
import type { ReactNode } from 'react'

import { ChartShell } from './chart-shell'
import { PLOT_TIME_BASELINE, plotArea } from './plot'

export interface BarSeries {
  id: string
  label: string
  /** A semantic token. Ignored when `signed`, where sign picks the tint. */
  color: string
  values: readonly number[]
}

export interface BarChartProps {
  label: string
  title?: string
  meta?: ReactNode
  categories: readonly string[]
  series: readonly BarSeries[]
  /** Colour each bar by its sign instead of its series. */
  signed?: boolean
  format: (value: number) => string
}

/** Bars stay thin; the slot's leftover width is air, not ink. */
const MAX_BAR_WIDTH = 24
const BAR_GAP = 2
const CAP_RADIUS = 4

/** A column with a rounded data end and a square baseline end. */
function columnPath(x: number, width: number, baseY: number, valueY: number) {
  const r = Math.min(CAP_RADIUS, width / 2, Math.abs(baseY - valueY))
  if (valueY <= baseY) {
    return `M${x} ${baseY}V${valueY + r}Q${x} ${valueY} ${x + r} ${valueY}H${x + width - r}Q${x + width} ${valueY} ${x + width} ${valueY + r}V${baseY}Z`
  }
  return `M${x} ${baseY}V${valueY - r}Q${x} ${valueY} ${x + r} ${valueY}H${x + width - r}Q${x + width} ${valueY} ${x + width} ${valueY - r}V${baseY}Z`
}

function BarPlot({
  width,
  height,
  categories,
  series,
  signed,
  format,
}: Pick<BarChartProps, 'categories' | 'series' | 'format'> & {
  width: number
  height: number
  signed: boolean
}) {
  const area = plotArea(width, height)
  const values = series.flatMap(line => line.values)
  const { domain, ticks } = niceDomain(Math.min(0, ...values), Math.max(0, ...values), 3)
  const yScale = createLinearScale(domain, [area.bottom, area.top])
  const slot = (area.right - area.left) / Math.max(1, categories.length)
  const barWidth = Math.min(MAX_BAR_WIDTH, (slot - BAR_GAP * (series.length + 1)) / series.length)
  const groupWidth = barWidth * series.length + BAR_GAP * (series.length - 1)
  const extreme = values.reduce(
    (best, value) => (Math.abs(value) > Math.abs(best) ? value : best),
    values[0] ?? 0
  )

  return (
    <svg aria-hidden="true" width={width} height={height} className="block overflow-clip">
      {ticks.map(tick => (
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
            {format(tick)}
          </text>
        </g>
      ))}
      {categories.map((category, column) => {
        const groupLeft = area.left + slot * column + (slot - groupWidth) / 2
        return (
          <g key={category}>
            {series.map((line, index) => {
              const value = line.values[column] ?? 0
              const x = groupLeft + index * (barWidth + BAR_GAP)
              const tint = signed ? (value >= 0 ? 'fill-positive' : 'fill-negative') : undefined
              const y = yScale(value)
              return (
                <g key={line.id}>
                  <path
                    d={columnPath(x, barWidth, yScale(0), y)}
                    className={tint}
                    style={tint ? undefined : { fill: line.color }}
                    fillOpacity={0.85}
                  />
                  {value === extreme ? (
                    <text
                      className="fill-foreground font-mono text-caption"
                      x={x + barWidth / 2}
                      y={value >= 0 ? y - 4 : y + 12}
                      textAnchor="middle"
                    >
                      {format(value)}
                    </text>
                  ) : null}
                </g>
              )
            })}
            <text
              className="fill-muted-foreground text-caption"
              x={groupLeft + groupWidth / 2}
              y={height - PLOT_TIME_BASELINE}
              textAnchor="middle"
            >
              {category}
            </text>
          </g>
        )
      })}
    </svg>
  )
}

export function BarChart({
  label,
  title,
  meta,
  categories,
  series,
  signed = false,
  format,
}: BarChartProps) {
  return (
    <ChartShell
      label={label}
      {...(title === undefined ? {} : { title })}
      value={meta}
      legend={
        series.length > 1
          ? series.map(line => ({ id: line.id, label: line.label, color: line.color }))
          : undefined
      }
    >
      {size => (
        <BarPlot
          width={size.width}
          height={size.height}
          categories={categories}
          series={series}
          signed={signed}
          format={format}
        />
      )}
    </ChartShell>
  )
}
