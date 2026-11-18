'use client'

import { useEffect, useEffectEvent, useId, useMemo, useRef, useState, type ReactNode } from 'react'

import { cn } from '../lib/cn'
import { ReferenceLine } from './chart-overlays'
import { CandleSeriesLayer } from './internal/candle-layer'
import { ChartGrid, ChartXAxis, ChartYAxis, type GridTone } from './internal/chart-axes'
import { ChartContext, type ChartContextValue } from './internal/chart-context'
import { createHoverStore } from './internal/hover-store'
import { CandleReadout } from './internal/candle-readout'
import { ChartDefinitions } from './internal/chart-definitions'
import { HoverAnnouncer, HoverLayer } from './internal/hover-layer'
import { buildHover } from './internal/hover-model'
import { layoutChart, resolveChartData } from './internal/geometry'
import { EndpointLabels, EndpointMarker, LineSeriesLayer } from './internal/line-layer'
import { useStableDomain } from './internal/use-stable-domain'
import { DOMAIN_ANIMATION_MS, useAnimatedDomain } from './internal/use-animated-domain'
import { useElementSize } from './internal/use-element-size'
import { useLiveSeries } from './internal/use-live-series'
import { useReveal } from './internal/use-reveal'
import { useStrokeSeries } from './internal/use-stroke-series'
import { useWallClock } from './internal/use-wall-clock'
import { chartKindLabels, formatValue } from './lib/format'
import type { LineCurve } from './lib/path'
import { TIP_LIVE_MAX_LEAD_MS, TIP_MAX_LEAD_MS } from './lib/stroke'
import type {
  Candle,
  ChartDensity,
  ChartHover,
  ChartKind,
  LineSeries,
  TimeRange,
  ValueFormat,
} from './lib/types'

export type {
  Candle,
  ChartDensity,
  ChartHover,
  ChartHoverValue,
  ChartKind,
  ChartPoint,
  LineSeries,
  TimeRange,
  ValueFormat,
} from './lib/types'
export type { LineCurve } from './lib/path'
export type { GridTone } from './internal/chart-axes'
export { CHART_KINDS } from './lib/types'

export interface MarketChartProps {
  /** Washes the space under each line with its colour; on for price charts by default. */
  area?: boolean | undefined
  /** Candle bucket length in milliseconds; chosen automatically when omitted. */
  candleInterval?: number | undefined
  /** Pre-aggregated candles; derived from the first series when omitted. */
  candles?: readonly Candle[] | undefined
  /** SVG overlays such as `<ReferenceLine>` rendered above the series. */
  children?: ReactNode
  className?: string | undefined
  /** Line interpolation; monotone cubic by default, which cannot overshoot a sample. */
  curve?: LineCurve | undefined
  /** `compact` for a chart inside a chat reply: tight insets, four rows, no hover pills. */
  density?: ChartDensity | undefined
  /**
   * Samples before this time (epoch ms) draw dimmed: context that led into the period
   * being charted, such as the run-up to a round's open, rather than part of it.
   */
  dimBefore?: number | undefined
  /** Renders the time label nearest the latest sample in the foreground colour. */
  emphasizeNowTick?: boolean | undefined
  /** Values `overlays` flags mark; the axis drops the tick labels they would sit on. */
  hideTickNear?: number | readonly number[] | undefined
  /** Gridline strength; derived from `kind` when omitted. */
  gridTone?: GridTone | undefined
  /** Fixed height in pixels; measured from the container when omitted. */
  height?: number | undefined
  kind: ChartKind
  /** Pulses the latest sample to signal streaming data. */
  live?: boolean | undefined
  locale?: string | undefined
  onHoverChange?: ((hover: ChartHover | null) => void) | undefined
  /** SVG marks drawn above the series and outside the plot clip, such as axis-gutter flags. */
  overlays?: ReactNode
  /** Visible time window; defaults to the data extent. */
  range?: TimeRange | undefined
  series: readonly LineSeries[]
  /** Large value labels beside each line end; on for probability charts by default. */
  showEndpointLabels?: boolean | undefined
  /** Value the y axis is centred on, drawn as a reference line. */
  target?: number | undefined
  targetLabel?: string | undefined
  timeZone?: string | undefined
  /** Accessible name for the figure; a summary of the latest values by default. */
  title?: string | undefined
  /**
   * Fraction of the plot width left empty past `range.end`, so the line ends short of the
   * axis gutter and its value flag. Ticks and hover keep following the scale into the gap.
   */
  trailingSpace?: number | undefined
  /** A fixed value axis for a bounded quantity such as RSI; the data never rescales it. */
  valueDomain?: readonly [number, number] | undefined
  valueFormat?: ValueFormat | undefined
  /** Fixed width in pixels; measured from the container when omitted. */
  width?: number | undefined
}

/** How long the value axis takes to reach a new fit, by density. */
const DOMAIN_ANIMATION_MS_BY_DENSITY: Record<ChartDensity, number> = {
  compact: 700,
  default: DOMAIN_ANIMATION_MS,
}

/** Extends the end of `range` so it lands `trailingSpace` of the plot width short of the edge. */
function stretchRange(range: TimeRange, trailingSpace: number): readonly [number, number] {
  const fraction = Math.min(0.9, Math.max(0, trailingSpace))

  return [range.start, range.start + (range.end - range.start) / (1 - fraction)]
}

/** The most recent sample time across every series. */
function latestSampleTime(series: readonly LineSeries[]): number | undefined {
  let latest: number | undefined

  for (const item of series) {
    const last = item.points.at(-1)

    if (last && (latest === undefined || last.time > latest)) {
      latest = last.time
    }
  }

  return latest
}

/**
 * Time-series market chart. Switch `kind` between `probability`, `price` and
 * `candlestick` to re-render the same data as glowing lines or OHLC candles.
 */
export function MarketChart({
  area,
  candleInterval,
  candles,
  children,
  className,
  curve = 'monotone',
  density = 'default',
  dimBefore,
  emphasizeNowTick = false,
  gridTone,
  height: fixedHeight,
  hideTickNear,
  kind,
  live = false,
  locale,
  onHoverChange,
  overlays,
  range,
  series,
  showEndpointLabels,
  target,
  targetLabel = 'Target',
  timeZone,
  title,
  trailingSpace = 0,
  valueDomain,
  valueFormat,
  width: fixedWidth,
}: MarketChartProps) {
  const containerRef = useRef<HTMLElement | null>(null)
  const measured = useElementSize(
    containerRef,
    fixedWidth === undefined || fixedHeight === undefined
  )
  const width = fixedWidth ?? measured?.width ?? 0
  const height = fixedHeight ?? measured?.height ?? 0
  const rawId = useId().replaceAll(/[^a-zA-Z0-9_-]/g, '')
  const glowFilterId = `tiny-chart-glow-${rawId}`
  const revealClipId = `tiny-chart-reveal-${rawId}`
  const plotClipId = `tiny-chart-plot-${rawId}`
  const [hover] = useState(createHoverStore)
  const data = useMemo(
    () =>
      resolveChartData({
        candleInterval,
        candles,
        density,
        kind,
        locale,
        range,
        series,
        live,
        target,
        timeZone,
        valueDomain,
        valueFormat,
      }),
    [
      candleInterval,
      candles,
      density,
      series,
      live,
      kind,
      locale,
      range,
      target,
      timeZone,
      valueDomain,
      valueFormat,
    ]
  )
  const domainKey = `${kind}:${data.valueFormat.style}`
  // A live chart's tip rides the wall clock, clamped to the window: a fixed round window
  // ends in the future, and a moving viewport already ends at the clock.
  const wallNow = useWallClock(live)
  const tipNow = live ? Math.min(wallNow, data.range.end) : data.range.end
  // Drawn form: a committed, smoothed grid that is never rewritten, with a sprung live tip.
  const displaySeries = useLiveSeries(
    useStrokeSeries(
      series,
      tipNow,
      kind !== 'candlestick',
      live ? TIP_LIVE_MAX_LEAD_MS : TIP_MAX_LEAD_MS
    ),
    live
  )
  const stableDomain = useStableDomain(data.yTarget.domain, domainKey, live)
  // A compact chart sits beside prose; its axis drifts rather than snaps when it re-fits.
  const yDomain = useAnimatedDomain(
    stableDomain,
    domainKey,
    true,
    DOMAIN_ANIMATION_MS_BY_DENSITY[density]
  )
  // A live window already moves with the clock; easing it a second time stutters.
  const xDomain = useAnimatedDomain(stretchRange(data.range, trailingSpace), kind, !live)
  const geometry = useMemo(
    () => ({
      ...layoutChart({
        data,
        glowFilterId,
        height,
        width,
        xDomain,
        yDomain,
      }),
      displaySeries,
    }),
    [data, displaySeries, glowFilterId, height, width, xDomain, yDomain]
  )
  const revealRectRef = useReveal(width + 40)

  const emitHover = useEffectEvent((next: ChartHover | null) => {
    onHoverChange?.(next)
  })

  useEffect(() => {
    return hover.subscribe(() => {
      const state = hover.get()
      emitHover(state ? buildHover(geometry, state) : null)
    })
  }, [geometry, hover])

  const lastCandle = data.candles.at(-1)
  const summary =
    kind === 'candlestick' && lastCandle
      ? `close ${formatValue(lastCandle.close, data.valueFormat, locale)}`
      : series
          .map(item => {
            const last = item.points.at(-1)

            return last
              ? `${item.label} ${formatValue(last.value, data.valueFormat, locale)}`
              : item.label
          })
          .join(', ')
  const hasData =
    kind === 'candlestick' ? data.candles.length > 0 : series.some(item => item.points.length > 0)
  const accessibleName = title ?? `${chartKindLabels[kind]} over time: ${summary}`
  const hasSize = width > 0 && height > 0
  const endpointLabels = showEndpointLabels ?? kind === 'probability'
  const nowTime = !emphasizeNowTick
    ? undefined
    : kind === 'candlestick'
      ? lastCandle?.time
      : latestSampleTime(series)
  const contextValue: ChartContextValue = { geometry, hover }

  return (
    <figure
      aria-label={accessibleName}
      className={cn('relative block h-full w-full min-h-0 min-w-0 select-none', className)}
      data-chart-kind={kind}
      ref={containerRef}
    >
      <figcaption className="sr-only">{accessibleName}</figcaption>
      {hasSize ? (
        <ChartContext.Provider value={contextValue}>
          <svg
            className="block overflow-visible"
            height={height}
            viewBox={`0 0 ${width} ${height}`}
            width={width}
          >
            <ChartDefinitions
              plotClipId={plotClipId}
              revealClipId={revealClipId}
              revealRectRef={revealRectRef}
            />

            <ChartGrid tone={gridTone ?? (kind === 'probability' ? 'strong' : 'subtle')} />
            {target !== undefined ? (
              <ReferenceLine dashed label={targetLabel} value={target} />
            ) : null}
            <ChartYAxis hideNear={hideTickNear} />
            <ChartXAxis emphasizeTime={nowTime} />

            <g clipPath={`url(#${plotClipId})`}>
              <g clipPath={`url(#${revealClipId})`}>
                <g className="motion-safe:animate-chart-fade-in" key={kind}>
                  {kind === 'candlestick' ? (
                    <CandleSeriesLayer />
                  ) : (
                    displaySeries.map(item => (
                      <LineSeriesLayer
                        area={area ?? kind === 'price'}
                        curve={curve}
                        dimBefore={dimBefore}
                        key={item.id}
                        series={item}
                      />
                    ))
                  )}
                </g>
                {kind === 'candlestick'
                  ? null
                  : displaySeries.map(item => (
                      <EndpointMarker key={item.id} live={live} series={item} />
                    ))}
                {children}
              </g>
            </g>

            {hasData ? null : (
              <text
                className="fill-muted-foreground text-label font-medium"
                data-chart-empty=""
                dominantBaseline="middle"
                textAnchor="middle"
                x={geometry.plot.left + geometry.plot.width / 2}
                y={geometry.plot.top + geometry.plot.height / 2}
              >
                No data yet
              </text>
            )}
            {overlays}
            {kind === 'candlestick' && hasData && density === 'default' ? <CandleReadout /> : null}
            {endpointLabels && kind !== 'candlestick' ? (
              <EndpointLabels series={displaySeries} values={series} />
            ) : null}

            <HoverLayer label={`${accessibleName}. Use arrow keys to move through time.`} />
          </svg>
          <HoverAnnouncer />
        </ChartContext.Provider>
      ) : null}
    </figure>
  )
}
