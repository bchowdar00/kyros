'use client'

import { formatValue } from '@tiny/ui/charts/lib/format'
import type { ChartHover, ChartPoint, ValueFormat } from '@tiny/ui/charts/lib/types'
import { MarketChart } from '@tiny/ui/charts/market-chart'
import { useLiveChartWindow } from '@tiny/ui/charts/use-live-chart-window'
import { type ReactNode, useState } from 'react'

import { ChartShell, type ChartShellWindow } from './chart-shell'
import {
  Band,
  Guides,
  Markers,
  type TimeSeriesBand,
  type TimeSeriesGuide,
  type TimeSeriesMarker,
} from './time-series-overlays'

export type { TimeSeriesBand, TimeSeriesGuide, TimeSeriesMarker }
export type TimeSeriesWindow = ChartShellWindow

export interface TimeSeriesLine {
  id: string
  label: string
  /** A semantic token, for example `var(--action)`. */
  color: string
  points: readonly ChartPoint[]
  /**
   * The last value in its own units, for the legend and header, when the plotted values
   * are not (a normalised comparison plots percent change but reads as a price).
   */
  lastLabel?: string
}

export interface TimeSeriesProps {
  label: string
  title?: string
  meta?: ReactNode
  series: readonly TimeSeriesLine[]
  format: ValueFormat
  guides?: readonly TimeSeriesGuide[]
  band?: TimeSeriesBand
  markers?: readonly TimeSeriesMarker[]
  windows?: readonly TimeSeriesWindow[]
  /** Fixed value domain, for bounded signals. Otherwise stable bounds around the data. */
  domain?: readonly [number, number]
  timeZone: string
  /** The feed is streaming: the window rolls with the clock and the tip is sprung. */
  live?: boolean
}

/** The line ends this fraction of the plot short of the axis gutter. */
const TRAILING_SPACE = 0.04

/**
 * One or more lines over time, inside the chart shell. The drawing is the market chart at
 * compact density, so history is a committed stroke that never moves, the value axis holds
 * through ordinary ticks, and a live window scrolls at clock speed.
 */
export function TimeSeries({
  label,
  title,
  meta,
  series,
  format,
  guides = [],
  band,
  markers = [],
  windows,
  domain,
  timeZone,
  live = false,
}: TimeSeriesProps) {
  const [windowIndex, setWindowIndex] = useState(0)
  const [hover, setHover] = useState<ChartHover | null>(null)
  const primary = series[0]
  const points = primary?.points ?? []
  const firstTime = points[0]?.time
  const lastTime = points.at(-1)?.time
  const extent = firstTime !== undefined && lastTime !== undefined ? lastTime - firstTime : 60_000
  const spanMs = windows?.[windowIndex]?.ms ?? Math.max(extent, 1_000)
  const { range } = useLiveChartWindow({ live, points, spanMs })

  const valueOf = (line: TimeSeriesLine) => {
    const hovered = hover?.values.find(value => value.seriesId === line.id)?.point.value
    if (hovered !== undefined) return formatValue(hovered, format)
    return line.lastLabel ?? formatValue(line.points.at(-1)?.value ?? Number.NaN, format)
  }
  const headerValue =
    meta ?? (primary !== undefined && series.length === 1 ? valueOf(primary) : undefined)

  return (
    <ChartShell
      label={label}
      {...(title === undefined ? {} : { title })}
      value={headerValue}
      legend={
        series.length > 1
          ? series.map(line => ({
              id: line.id,
              label: line.label,
              color: line.color,
              value: valueOf(line),
            }))
          : undefined
      }
      windows={windows}
      windowIndex={windowIndex}
      onWindowChange={setWindowIndex}
    >
      {size => (
        <MarketChart
          kind="price"
          density="compact"
          area={false}
          live={live}
          series={series}
          range={range}
          valueFormat={format}
          valueDomain={domain}
          timeZone={timeZone}
          title={label}
          trailingSpace={TRAILING_SPACE}
          // A labelled guide owns its spot in the axis gutter; the tick there would collide.
          hideTickNear={guides.filter(guide => guide.label !== undefined).map(guide => guide.value)}
          emphasizeNowTick={live}
          onHoverChange={setHover}
          width={size.width}
          height={size.height}
        >
          {band ? <Band band={band} /> : null}
          <Guides guides={guides} />
          <Markers markers={markers} />
        </MarketChart>
      )}
    </ChartShell>
  )
}
