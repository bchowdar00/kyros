'use client'

import { ReferenceLine, TimeMarker, useChartGeometry } from '@kyros/ui/charts/chart-overlays'
import { buildAreaPath, buildLinePath } from '@kyros/ui/charts/lib/path'
import { sliceToRange } from '@kyros/ui/charts/lib/scale'
import type { LineSeries } from '@kyros/ui/charts/lib/types'
import { useId } from 'react'

export interface TimeSeriesGuide {
  value: number
  label?: string
}

export type TimeSeriesBand =
  | { kind: 'between'; a: string; b: string; color: string }
  | { kind: 'threshold'; seriesId: string; value: number; side: 'above' | 'below'; color: string }

export interface TimeSeriesMarker {
  time: number
  label: string
  /** Which line the marker's dot sits on. Defaults to the first series. */
  seriesId?: string
}

const BAND_OPACITY = 0.14

export function Guides({ guides }: { guides: readonly TimeSeriesGuide[] }) {
  return guides.map(guide => (
    <ReferenceLine
      key={guide.value}
      dashed
      value={guide.value}
      {...(guide.label === undefined ? {} : { label: guide.label })}
    />
  ))
}

export function Markers({ markers }: { markers: readonly TimeSeriesMarker[] }) {
  return markers.map(marker => (
    <TimeMarker key={`${marker.time}-${marker.label}`} time={marker.time} label={marker.label} />
  ))
}

/** The drawn stroke of a series in plot pixels, within the visible window. */
function useStrokePixels(seriesId: string) {
  const { displaySeries, series, visibleRange, xScale, yScale } = useChartGeometry()
  const drawn: LineSeries | undefined =
    displaySeries?.find(item => item.id === seriesId) ?? series.find(item => item.id === seriesId)
  if (!drawn) return []
  return sliceToRange(drawn.points, visibleRange)
    .filter(point => Number.isFinite(point.value))
    .map(point => ({ x: xScale(point.time), y: yScale(point.value) }))
}

/**
 * Shade between a line and a level on one side of it, or between two lines. Drawn from the
 * same committed stroke as the line, so the wash moves only where the tip moves.
 */
export function Band({ band }: { band: TimeSeriesBand }) {
  return band.kind === 'threshold' ? <ThresholdBand band={band} /> : <BetweenBand band={band} />
}

function ThresholdBand({ band }: { band: Extract<TimeSeriesBand, { kind: 'threshold' }> }) {
  const clipId = useId()
  const { plot, yScale } = useChartGeometry()
  const pixels = useStrokePixels(band.seriesId)
  if (pixels.length < 2) return null
  const level = yScale(band.value)
  const clipTop = band.side === 'above' ? plot.top : level
  const clipBottom = band.side === 'above' ? level : plot.bottom
  return (
    <g clipPath={`url(#${clipId})`} data-chart-band={band.side}>
      <clipPath id={clipId}>
        <rect
          x={plot.left}
          width={plot.width}
          y={clipTop}
          height={Math.max(0, clipBottom - clipTop)}
        />
      </clipPath>
      <path
        d={buildAreaPath(pixels, level, 'monotone')}
        style={{ fill: band.color }}
        fillOpacity={BAND_OPACITY}
      />
    </g>
  )
}

function BetweenBand({ band }: { band: Extract<TimeSeriesBand, { kind: 'between' }> }) {
  const a = useStrokePixels(band.a)
  const b = useStrokePixels(band.b)
  if (a.length < 2 || b.length < 2) return null
  const forward = buildLinePath(a, 'monotone')
  const backward = buildLinePath(b.toReversed(), 'monotone').replace(/^M/, 'L')
  return (
    <path
      data-chart-band="between"
      d={`${forward}${backward}Z`}
      style={{ fill: band.color }}
      fillOpacity={BAND_OPACITY}
    />
  )
}
