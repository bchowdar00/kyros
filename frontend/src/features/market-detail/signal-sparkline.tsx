'use client'

import type { ChartPoint } from '@kyros/ui/charts/lib/types'
import { useEffect, useRef } from 'react'

export interface SignalSparklineProps {
  /** Samples in time order; fewer than two draw as a single dot at the latest value. */
  points: readonly ChartPoint[]
  /** Height and width; the row's small slot by default. */
  className?: string
  /** The colour token the trace is drawn in; the quiet text colour by default. */
  colorVar?: `--color-${string}`
}

/** Fewer samples than this, or a flat line, says nothing worth drawing. */
const MIN_SAMPLES_FOR_TRACE = 3

/** Whether a trace of these points would show a value that actually moved. */
export function hasMovement(points: readonly ChartPoint[]): boolean {
  if (points.length < MIN_SAMPLES_FOR_TRACE) return false
  const first = points[0]?.value
  return points.some(point => point.value !== first)
}

const MAX_DEVICE_PIXEL_RATIO = 2
const INSET = 2
const STROKE_WIDTH = 1.25
const DOT_RADIUS = 1.5

/** A thin trace of a row's last minute, drawn on canvas as the chart rules require. */
export function SignalSparkline({
  points,
  className = 'block h-2xl w-full',
  colorVar = '--color-muted-foreground',
}: SignalSparklineProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null)

  useEffect(() => {
    const canvas = canvasRef.current
    if (!canvas || typeof CanvasRenderingContext2D === 'undefined') return
    const context = canvas.getContext('2d')
    if (!context) return

    const bounds = canvas.getBoundingClientRect()
    const pixelRatio = Math.min(window.devicePixelRatio || 1, MAX_DEVICE_PIXEL_RATIO)
    const width = Math.round(bounds.width * pixelRatio)
    const height = Math.round(bounds.height * pixelRatio)
    if (canvas.width !== width || canvas.height !== height) {
      canvas.width = width
      canvas.height = height
    }
    context.clearRect(0, 0, width, height)

    const last = points.at(-1)
    if (last === undefined) return

    const color = getComputedStyle(canvas).getPropertyValue(colorVar).trim()
    const inset = INSET * pixelRatio
    const plotWidth = width - inset * 2
    const plotHeight = height - inset * 2
    const first = points[0] ?? last
    const timeRange = Math.max(last.time - first.time, 1)
    let minimum = Number.POSITIVE_INFINITY
    let maximum = Number.NEGATIVE_INFINITY
    for (const point of points) {
      minimum = Math.min(minimum, point.value)
      maximum = Math.max(maximum, point.value)
    }
    const range = maximum - minimum
    const x = (point: ChartPoint) => inset + ((point.time - first.time) / timeRange) * plotWidth
    const y = (point: ChartPoint) =>
      range === 0
        ? inset + plotHeight / 2
        : inset + (1 - (point.value - minimum) / range) * plotHeight

    context.strokeStyle = color
    context.fillStyle = color
    context.lineWidth = STROKE_WIDTH * pixelRatio
    context.lineCap = 'round'
    context.lineJoin = 'round'

    if (points.length > 1) {
      context.beginPath()
      for (const [index, point] of points.entries()) {
        if (index === 0) context.moveTo(x(point), y(point))
        else context.lineTo(x(point), y(point))
      }
      context.stroke()
    }
    context.beginPath()
    context.arc(x(last), y(last), DOT_RADIUS * pixelRatio, 0, Math.PI * 2)
    context.fill()
  }, [points, colorVar])

  return <canvas aria-hidden="true" className={className} ref={canvasRef} />
}
