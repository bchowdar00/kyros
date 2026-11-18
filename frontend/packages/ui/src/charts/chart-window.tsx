export interface ChartWindow {
  /** Candle bucket used when the chart renders this time range as candlesticks. */
  candleInterval: number
  durationMs: number
}

const DEFAULT_CHART_WINDOW_INDEX = 1

/** Keeps the chart focused on its primary range without exposing time controls. */
export function resolveDefaultChartWindow(windows: readonly ChartWindow[]) {
  return windows[DEFAULT_CHART_WINDOW_INDEX] ?? windows[0]
}
