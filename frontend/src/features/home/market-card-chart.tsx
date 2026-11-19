'use client'

import { useEffect, useRef } from 'react'

import type { ChartPoint } from '@kyros/ui/charts/lib/types'

export interface MarketCardChartLine {
  colorToken: string
  id: string
  points: readonly ChartPoint[]
}

export interface MarketCardChartProps {
  label: string
  series: readonly MarketCardChartLine[]
}

const MAX_DEVICE_PIXEL_RATIO = 2
const HORIZONTAL_INSET = 2
const VERTICAL_INSET = 4

/** A lightweight decorative canvas plot for the compact home-market card. */
export function MarketCardChart({ label, series }: MarketCardChartProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null)
  const chartAvailable = series.some(lineSeries => lineSeries.points.length > 1)

  useEffect(() => {
    const canvas = canvasRef.current

    if (!canvas || !chartAvailable || typeof CanvasRenderingContext2D === 'undefined') {
      return
    }

    const updateCanvas = () => {
      const bounds = canvas.getBoundingClientRect()
      const pixelRatio = Math.min(window.devicePixelRatio || 1, MAX_DEVICE_PIXEL_RATIO)
      const width = Math.round(bounds.width * pixelRatio)
      const height = Math.round(bounds.height * pixelRatio)
      const context = canvas.getContext('2d')

      if (!context) {
        return
      }

      if (canvas.width !== width || canvas.height !== height) {
        canvas.width = width
        canvas.height = height
      }

      const values = series.flatMap(lineSeries => lineSeries.points.map(point => point.value))
      const minimum = Math.min(...values)
      const maximum = Math.max(...values)
      const range = Math.max(maximum - minimum, 1)
      const padding = range * 0.08
      const plotWidth = width - HORIZONTAL_INSET * 2
      const plotHeight = height - VERTICAL_INSET * 2
      const style = getComputedStyle(canvas)

      context.clearRect(0, 0, width, height)
      context.globalCompositeOperation = 'screen'
      context.lineCap = 'round'
      context.lineJoin = 'round'

      for (const lineSeries of series) {
        const firstPoint = lineSeries.points[0]
        const lastPoint = lineSeries.points.at(-1)

        if (!firstPoint || !lastPoint) {
          continue
        }

        const timeRange = Math.max(lastPoint.time - firstPoint.time, 1)
        const color = style.getPropertyValue(lineSeries.colorToken).trim()
        context.beginPath()

        for (const [pointIndex, point] of lineSeries.points.entries()) {
          const x = HORIZONTAL_INSET + ((point.time - firstPoint.time) / timeRange) * plotWidth
          const y =
            VERTICAL_INSET +
            (1 - (point.value - minimum + padding) / (range + padding * 2)) * plotHeight

          if (pointIndex === 0) {
            context.moveTo(x, y)
          } else {
            context.lineTo(x, y)
          }
        }

        context.strokeStyle = color
        context.globalAlpha = 0.28
        context.lineWidth = 4
        context.stroke()
        context.globalAlpha = 1
        context.lineWidth = 1.5
        context.stroke()
      }
    }

    if (typeof ResizeObserver === 'undefined') {
      updateCanvas()

      return
    }

    const observer = new ResizeObserver(updateCanvas)
    observer.observe(canvas)
    updateCanvas()

    return () => observer.disconnect()
  }, [chartAvailable, series])

  if (!chartAvailable) {
    return <p className="text-body text-muted-foreground">Chart unavailable</p>
  }

  return (
    <figure aria-label={label} className="size-full" data-slot="market-card-plot">
      <canvas aria-hidden="true" className="block size-full" ref={canvasRef} />
    </figure>
  )
}
