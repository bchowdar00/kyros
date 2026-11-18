'use client'

import { createContext, useContext } from 'react'

import type { LinearScale } from '../lib/scale'
import type { TimeTicks } from '../lib/time'
import type {
  Candle,
  ChartDensity,
  ChartKind,
  LineSeries,
  TimeRange,
  ValueFormat,
} from '../lib/types'
import type { HoverStore } from './hover-store'

export interface PlotArea {
  bottom: number
  height: number
  left: number
  right: number
  top: number
  width: number
}

export interface ChartGeometry {
  displaySeries?: readonly LineSeries[]
  /** Format used for axis tick labels (currency symbols are dropped there). */
  axisFormat: ValueFormat
  axisFontSize: number
  /** Candle bucket length in milliseconds, `0` for line charts. */
  candleInterval: number
  /** Width of one candle bucket in pixels, `0` for line charts. */
  candleWidth: number
  candles: readonly Candle[]
  /** x position of the most recent sample. */
  dataEndX: number
  density: ChartDensity
  glowFilterId: string
  height: number
  kind: ChartKind
  locale: string | undefined
  plot: PlotArea
  range: TimeRange
  series: readonly LineSeries[]
  timeZone: string | undefined
  valueFormat: ValueFormat
  /** The time span currently mapped onto the plot; glides towards `range` after a window change. */
  visibleRange: TimeRange
  width: number
  xScale: LinearScale
  xTicks: TimeTicks
  yScale: LinearScale
  yTicks: readonly number[]
}

export interface ChartContextValue {
  geometry: ChartGeometry
  hover: HoverStore
}

export const ChartContext = createContext<ChartContextValue | null>(null)

export function useChartContext(): ChartContextValue {
  const value = useContext(ChartContext)

  if (!value) {
    throw new Error('Chart components must be rendered inside <MarketChart>.')
  }

  return value
}

/** Scales, plot bounds and formatting for the surrounding chart; use it to draw custom overlays. */
export function useChartGeometry(): ChartGeometry {
  return useChartContext().geometry
}
