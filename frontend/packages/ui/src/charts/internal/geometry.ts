import { aggregateCandles } from '../lib/candles'
import { estimateTextWidth, formatValue, PERCENT_FORMAT, USD_FORMAT } from '../lib/format'
import {
  centeredDomain,
  createLinearScale,
  type DomainWithTicks,
  linearTicks,
  sliceToRange,
  niceDomain,
} from '../lib/scale'
import { chooseTimeStep, timeTicks } from '../lib/time'
import type {
  Candle,
  ChartDensity,
  ChartKind,
  LineSeries,
  TimeRange,
  ValueFormat,
} from '../lib/types'
import type { ChartGeometry, PlotArea } from './chart-context'

export interface ChartDataInput {
  candleInterval: number | undefined
  candles: readonly Candle[] | undefined
  density?: ChartDensity
  kind: ChartKind
  live?: boolean
  locale: string | undefined
  range: TimeRange | undefined
  series: readonly LineSeries[]
  target: number | undefined
  timeZone: string | undefined
  /** A fixed value axis for a bounded quantity such as RSI; the data never rescales it. */
  valueDomain?: readonly [number, number] | undefined
  valueFormat: ValueFormat | undefined
}

export interface ResolvedChartData {
  axisFontSize: number
  axisFormat: ValueFormat
  candleInterval: number
  candles: readonly Candle[]
  density: ChartDensity
  /** The caller fixed the value axis, so its ticks are the grid rather than evenly spaced rows. */
  fixedTicks: boolean
  kind: ChartKind
  live?: boolean
  locale: string | undefined
  range: TimeRange
  series: readonly LineSeries[]
  timeZone: string | undefined
  valueFormat: ValueFormat
  yTarget: DomainWithTicks
}

const PROBABILITY_DOMAIN: DomainWithTicks = {
  domain: [0, 100],
  step: 25,
  ticks: [0, 25, 50, 75, 100],
}

const AUTO_CANDLE_COUNT = 90
const RANGE_PADDING = 0.04
// Half a basis point leaves quiet BTC ticks legible without a mostly empty $30–40 range.
const LIVE_MINIMUM_SPAN_RATIO = 0.00005
// Room above and below a live range, as a fraction of it, before the axis has to re-fit.
const LIVE_HEADROOM = 0.2

function dataRange(series: readonly LineSeries[], candles: readonly Candle[], interval: number) {
  let start = Number.POSITIVE_INFINITY
  let end = Number.NEGATIVE_INFINITY

  for (const item of series) {
    const first = item.points[0]
    const last = item.points.at(-1)

    if (first && last) {
      start = Math.min(start, first.time)
      end = Math.max(end, last.time)
    }
  }

  for (const candle of candles) {
    start = Math.min(start, candle.time)
    end = Math.max(end, candle.time + interval)
  }

  if (!Number.isFinite(start) || !Number.isFinite(end)) {
    const now = Date.now()

    return { end: now, start: now - 60 * 60 * 1000 }
  }

  return { end: end === start ? start + 60_000 : end, start }
}

function toAxisFormat(format: ValueFormat): ValueFormat {
  if (format.style !== 'currency') {
    return format
  }

  // Compact currency keeps its symbol on the axis: $60B reads better than 60,000,000,000.
  if (format.notation === 'compact') {
    return format
  }

  const maximumFractionDigits = format.maximumFractionDigits ?? 2

  return {
    maximumFractionDigits,
    // Axis ticks keep meaningful fractions without padding whole-dollar labels.
    minimumFractionDigits: 0,
    style: 'number',
  }
}

function collectValues(
  series: readonly LineSeries[],
  candles: readonly Candle[],
  interval: number,
  range: TimeRange
) {
  const values: number[] = []

  for (const item of series) {
    for (const point of sliceToRange(item.points, range)) {
      if (Number.isFinite(point.value)) {
        values.push(point.value)
      }
    }
  }

  for (const candle of candles) {
    const inRange = candle.time + interval > range.start && candle.time < range.end

    if (inRange && Number.isFinite(candle.low) && Number.isFinite(candle.high)) {
      values.push(candle.low, candle.high)
    }
  }

  return values
}

/** Everything about a chart that does not depend on its pixel size. */
export function resolveChartData(input: ChartDataInput): ResolvedChartData {
  const { kind, locale, series, target, timeZone } = input
  const density = input.density ?? 'default'
  const valueFormat = input.valueFormat ?? (kind === 'probability' ? PERCENT_FORMAT : USD_FORMAT)
  const primaryPoints = series[0]?.points ?? []
  const provisionalRange =
    input.range ?? dataRange(series, input.candles ?? [], input.candleInterval ?? 0)
  const candleInterval =
    input.candleInterval ??
    chooseTimeStep(provisionalRange.end - provisionalRange.start, AUTO_CANDLE_COUNT)
  const candles =
    kind === 'candlestick' ? (input.candles ?? aggregateCandles(primaryPoints, candleInterval)) : []
  const range = input.range ?? dataRange(series, candles, candleInterval)
  const values = collectValues(kind === 'candlestick' ? [] : series, candles, candleInterval, range)

  let yTarget: DomainWithTicks

  if (input.valueDomain !== undefined) {
    const [low, high] = input.valueDomain
    const ticks = linearTicks(low, high, density === 'compact' ? 3 : 5)
    yTarget = {
      domain: [low, high],
      step: ticks.length > 1 ? (ticks[1] ?? 0) - (ticks[0] ?? 0) : high - low,
      ticks,
    }
  } else if (kind === 'probability' && target === undefined) {
    yTarget = PROBABILITY_DOMAIN
  } else if (input.live && (values.length > 0 || target !== undefined)) {
    // A live axis breathes slowly: generous headroom so a new high does not re-fit the
    // axis, and a minimum range so quiet ticks do not magnify into full-height jumps.
    const anchored = target === undefined ? values : [target, ...values]
    const low = Math.min(...anchored)
    const high = Math.max(...anchored)
    const scale = Math.abs(target ?? (low + high) / 2)
    const minimumSpan = Math.max(scale * LIVE_MINIMUM_SPAN_RATIO, 1)
    const span = Math.max(high - low, minimumSpan)
    const padding = (span - (high - low)) / 2 + span * LIVE_HEADROOM
    yTarget = niceDomain(low - padding, high + padding, 5)
  } else if (target !== undefined) {
    yTarget = centeredDomain(target, values)
  } else if (values.length === 0) {
    yTarget = { domain: [0, 1], step: 1, ticks: [] }
  } else {
    let min = Number.POSITIVE_INFINITY
    let max = Number.NEGATIVE_INFINITY

    for (const value of values) {
      min = Math.min(min, value)
      max = Math.max(max, value)
    }

    const span = max - min
    yTarget = niceDomain(min - span * RANGE_PADDING, max + span * RANGE_PADDING, 5)
  }

  return {
    axisFontSize: kind === 'probability' || density === 'compact' ? 10 : 12,
    axisFormat: toAxisFormat(valueFormat),
    candleInterval,
    candles,
    density,
    fixedTicks: input.valueDomain !== undefined,
    kind,
    locale,
    range,
    series,
    timeZone,
    valueFormat,
    yTarget,
  }
}

export interface LayoutInput {
  data: ResolvedChartData
  glowFilterId: string
  height: number
  width: number
  /** Time span mapped onto the plot; animated separately from `data.range`. */
  xDomain: readonly [number, number]
  yDomain: readonly [number, number]
}

// Horizontal grid levels stay in place while their prices follow the animated scale.
const PRICE_GRID_ROWS: Record<ChartDensity, number> = { compact: 4, default: 6 }
const Y_AXIS_GAP = 10
// Below the plot: the axis label row, plus on the market screen the hover time pill above it.
const X_AXIS_EXTRA: Record<ChartDensity, number> = { compact: 6, default: 40 }
// Above the plot: room for the hover value tag on the market screen; a hairline in a reply.
const PLOT_TOP: Record<ChartDensity, number> = { compact: 4, default: 38 }

export function layoutChart({
  data,
  glowFilterId,
  height,
  width,
  xDomain,
  yDomain,
}: LayoutInput): ChartGeometry {
  const {
    axisFontSize,
    axisFormat,
    candleInterval,
    candles,
    density,
    kind,
    locale,
    range,
    series,
  } = data
  // Price flags overlay the edge; reserve only the width needed by the actual axis ticks.
  // Probability labels still occupy a separate column from their axis ticks.
  let yAxisWidth = kind === 'probability' ? 140 : 0

  for (const tick of data.yTarget.ticks) {
    yAxisWidth = Math.max(
      yAxisWidth,
      estimateTextWidth(formatValue(tick, axisFormat, locale), axisFontSize)
    )
  }

  const plotTop = kind === 'probability' ? Math.ceil(axisFontSize / 2) + 2 : PLOT_TOP[density]
  const plot: PlotArea = {
    bottom: height - (axisFontSize + X_AXIS_EXTRA[density]),
    height: 0,
    left: 0,
    right: Math.max(1, width - Math.ceil(yAxisWidth + Y_AXIS_GAP)),
    top: plotTop,
    width: 0,
  }
  plot.width = Math.max(1, plot.right - plot.left)
  plot.height = Math.max(1, plot.bottom - plot.top)

  const visibleRange: TimeRange = { end: xDomain[1], start: xDomain[0] }
  const xScale = createLinearScale(xDomain, [plot.left, plot.right])
  const yScale = createLinearScale(yDomain, [plot.bottom, plot.top])
  const candleWidth =
    kind === 'candlestick'
      ? xScale(visibleRange.start + candleInterval) - xScale(visibleRange.start)
      : 0
  let dataEndX = plot.left

  if (kind === 'candlestick') {
    const last = candles.at(-1)

    if (last) {
      dataEndX = xScale(last.time + candleInterval)
    }
  } else {
    for (const item of series) {
      const last = item.points.at(-1)

      if (last) {
        dataEndX = Math.max(dataEndX, xScale(last.time))
      }
    }
  }

  return {
    axisFontSize,
    axisFormat,
    candleInterval,
    candleWidth,
    candles,
    dataEndX: Math.min(dataEndX, plot.right),
    density,
    glowFilterId,
    height,
    kind,
    locale,
    plot,
    range,
    series,
    timeZone: data.timeZone,
    valueFormat: data.valueFormat,
    visibleRange,
    width,
    xScale,
    xTicks: timeTicks(range, {
      maxTicks: Math.max(2, Math.floor(plot.width / 150) + 1),
      timeZone: data.timeZone,
    }),
    yScale,
    yTicks:
      kind !== 'probability' && !data.fixedTicks && data.yTarget.ticks.length > 0
        ? Array.from(
            { length: PRICE_GRID_ROWS[density] },
            (_, row) =>
              yDomain[0] + ((yDomain[1] - yDomain[0]) * row) / (PRICE_GRID_ROWS[density] - 1)
          )
        : data.yTarget.ticks,
  }
}
