export interface ChartFeed {
  state: 'disconnected' | 'live' | 'paused'
}

export const CHART_KINDS = ['probability', 'price', 'candlestick'] as const

export type ChartKind = (typeof CHART_KINDS)[number]

/**
 * How much room the chart gives its chrome. `default` is the market screen: a hover
 * time pill under the axis, six grid rows, 12px axis labels. `compact` is a chart
 * inside a chat reply: tight insets, four rows, 10px labels, and no hover pills.
 */
export type ChartDensity = 'default' | 'compact'

/** A single time-series sample. `time` is a Unix epoch in milliseconds. */
export interface ChartPoint {
  time: number
  value: number
  /** Optional traded volume for this sample; summed when aggregating candles. */
  volume?: number
}

export interface LineSeries {
  /** Any CSS color, preferably a semantic token such as `var(--market-home)`. */
  color: string
  /** Stable identifier, used for React keys and hover callbacks. */
  id: string
  label: string
  points: readonly ChartPoint[]
}

export interface Candle {
  close: number
  high: number
  low: number
  open: number
  /** Bucket start, Unix epoch in milliseconds. */
  time: number
  volume?: number
}

export interface TimeRange {
  end: number
  start: number
}

export type ValueFormat =
  | { maximumFractionDigits?: number; style: 'percent' }
  | {
      currency?: string
      maximumFractionDigits?: number
      minimumFractionDigits?: number
      /** `compact` renders 31_460_000_000 as $31.46B, for volume-scale axes. */
      notation?: 'compact' | 'standard'
      style: 'currency'
    }
  | {
      maximumFractionDigits?: number
      minimumFractionDigits?: number
      notation?: 'compact' | 'standard'
      style: 'number'
    }

export interface ChartHoverValue {
  color: string
  label: string
  point: ChartPoint
  seriesId: string
}

export interface ChartHover {
  candle?: Candle
  time: number
  values: readonly ChartHoverValue[]
}
