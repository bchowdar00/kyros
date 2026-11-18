'use client'

import { useEffect, useRef } from 'react'

import { formatValue } from '../lib/format'
import { buildAreaPath, buildLinePath, type LineCurve } from '../lib/path'
import { findNearestIndex, sliceToRange } from '../lib/scale'
import type { LineSeries } from '../lib/types'
import { useChartContext, useChartGeometry } from './chart-context'
import { useHoverState } from './hover-store'
import { REVEAL_MS } from './use-reveal'

/** Canvas owns the data path; SVG is reserved for accessible axes and overlays. */
const AREA_OPACITY = 0.16

/** Stroke opacity for the run-up before `dimBefore`; the cursor never changes the stroke. */
const DIM_OPACITY = 0.28

export interface LineSeriesLayerProps {
  area?: boolean
  curve: LineCurve
  /** Samples before this time draw at `DIM_OPACITY`; see `MarketChartProps.dimBefore`. */
  dimBefore?: number | undefined
  series: LineSeries
}

export function LineSeriesLayer({ area = false, curve, dimBefore, series }: LineSeriesLayerProps) {
  const { geometry } = useChartContext()
  const canvasRef = useRef<HTMLCanvasElement | null>(null)

  useEffect(() => {
    if (typeof Path2D === 'undefined') return
    const canvas = canvasRef.current
    const context = canvas?.getContext('2d')
    if (!canvas || !context) return

    const { height, width, plot, visibleRange, xScale, yScale } = geometry
    const ratio = window.devicePixelRatio || 1
    if (
      canvas.width !== Math.round(width * ratio) ||
      canvas.height !== Math.round(height * ratio)
    ) {
      canvas.width = Math.round(width * ratio)
      canvas.height = Math.round(height * ratio)
    }
    context.setTransform(ratio, 0, 0, ratio, 0, 0)
    context.clearRect(0, 0, width, height)
    // `series` is already the drawn form (see useStrokeSeries): an even, committed grid
    // through a monotone curve, so history keeps its shape as the window scrolls.
    const visible = sliceToRange(series.points, visibleRange).filter(point =>
      Number.isFinite(point.value)
    )
    const pixels = visible.map(point => ({ x: xScale(point.time), y: yScale(point.value) }))
    if (pixels.length < 2) return
    const color = getComputedStyle(canvas).color
    const path = new Path2D(buildLinePath(pixels, curve))
    context.save()
    context.beginPath()
    context.rect(plot.left, plot.top, plot.width, plot.height)
    context.clip()
    if (area) {
      const areaTop = Math.max(
        plot.top,
        pixels.reduce((top, point) => Math.min(top, point.y), plot.bottom)
      )
      const gradient = context.createLinearGradient(0, areaTop, 0, plot.bottom)
      gradient.addColorStop(0, color)
      gradient.addColorStop(1, 'transparent')
      context.fillStyle = gradient
      context.globalAlpha = AREA_OPACITY
      // oxlint-disable-next-line unicorn/no-array-fill-with-reference-type -- CanvasRenderingContext2D.fill, not Array.fill
      context.fill(new Path2D(buildAreaPath(pixels, plot.bottom, curve)))
    }
    context.lineWidth = 2
    context.lineCap = 'round'
    context.lineJoin = 'round'
    context.strokeStyle = color
    // Hovering never recolours the stroke: the crosshair alone marks the cursor. Only the
    // run-up before `dimBefore` draws dimmed, as the same continuous stroke.
    const litLeft = dimBefore === undefined ? plot.left : Math.max(plot.left, xScale(dimBefore))
    const fullyLit = litLeft <= plot.left
    context.globalAlpha = fullyLit ? 1 : DIM_OPACITY
    context.stroke(path)
    if (!fullyLit && plot.right > litLeft) {
      context.beginPath()
      context.rect(litLeft, plot.top, plot.right - litLeft, plot.height)
      context.clip()
      context.globalAlpha = 1
      context.stroke(path)
    }
    context.restore()
  }, [area, curve, dimBefore, geometry, series.points])

  return (
    <foreignObject
      aria-hidden="true"
      data-chart-series={series.id}
      height={geometry.height}
      pointerEvents="none"
      width={geometry.width}
    >
      <canvas className="h-full w-full" ref={canvasRef} style={{ color: series.color }} />
    </foreignObject>
  )
}

export interface EndpointMarkerProps {
  live: boolean
  series: LineSeries
}

export function EndpointMarker({ live, series }: EndpointMarkerProps) {
  const { glowFilterId, xScale, yScale } = useChartGeometry()
  const last = series.points.at(-1)

  if (!last) {
    return null
  }

  const x = xScale(last.time)
  const y = yScale(last.value)

  return (
    <g
      className="motion-safe:animate-chart-fade-in"
      data-chart-endpoint={series.id}
      style={{ animationDelay: `${REVEAL_MS - 250}ms` }}
    >
      {live ? (
        <circle
          className="origin-center [transform-box:fill-box] motion-safe:animate-chart-pulse"
          cx={x}
          cy={y}
          fill={series.color}
          r={4}
        />
      ) : null}
      <circle cx={x} cy={y} fill={series.color} filter={`url(#${glowFilterId})`} r={4} />
      <circle cx={x} cy={y} fill={series.color} r={4} />
    </g>
  )
}

interface LabelPlacement {
  color: string
  id: string
  text: string
  y: number
}

const LABEL_GAP = 32
const LABEL_OFFSET_X = 14

/** Two-pass push-down / pull-up solver so stacked labels never overlap. */
export function resolveLabelCollisions(
  labels: readonly LabelPlacement[],
  top: number,
  bottom: number,
  gap = LABEL_GAP
): LabelPlacement[] {
  const sorted = labels.map(label => ({ ...label })).toSorted((a, b) => a.y - b.y)
  const half = gap / 2

  for (let index = 0; index < sorted.length; index += 1) {
    const current = sorted[index]
    const previous = sorted[index - 1]

    if (!current) {
      continue
    }

    current.y = Math.max(current.y, top + half)

    if (previous) {
      current.y = Math.max(current.y, previous.y + gap)
    }
  }

  for (let index = sorted.length - 1; index >= 0; index -= 1) {
    const current = sorted[index]
    const next = sorted[index + 1]

    if (!current) {
      continue
    }

    current.y = Math.min(current.y, bottom - half)

    if (next) {
      current.y = Math.min(current.y, next.y - gap)
    }
  }

  return sorted
}

export interface EndpointLabelsProps {
  /** Drawn series; a resting label sits at the end of its stroke. */
  series: readonly LineSeries[]
  /** Observed series; label text and hovered positions always read these. */
  values: readonly LineSeries[]
}

/** Large value labels beside each line end; they follow the hovered sample while scrubbing. */
export function EndpointLabels({ series, values }: EndpointLabelsProps) {
  const { geometry, hover } = useChartContext()
  const hoverState = useHoverState(hover)
  const { locale, plot, valueFormat, yScale } = geometry

  const placements = resolveLabelCollisions(
    values.flatMap(item => {
      const index = hoverState ? findNearestIndex(item.points, hoverState.time) : -1
      const point = index >= 0 ? item.points[index] : item.points.at(-1)
      const drawn = series.find(drawnItem => drawnItem.id === item.id)?.points.at(-1)

      if (!point) {
        return []
      }

      return [
        {
          color: item.color,
          id: item.id,
          text: formatValue(point.value, valueFormat, locale),
          y: yScale(index >= 0 || !drawn ? point.value : drawn.value),
        },
      ]
    }),
    plot.top,
    plot.bottom
  )

  return (
    <g
      className="motion-safe:animate-chart-fade-in"
      data-chart-endpoint-labels=""
      style={{ animationDelay: `${REVEAL_MS - 200}ms` }}
    >
      {placements.map(placement => {
        const x = plot.right + LABEL_OFFSET_X

        return (
          <g data-chart-endpoint-label={placement.id} key={placement.id}>
            <text
              className="text-2xl font-regular font-mono"
              dominantBaseline="middle"
              fill={placement.color}
              x={x}
              y={placement.y}
            >
              {placement.text}
            </text>
          </g>
        )
      })}
    </g>
  )
}
