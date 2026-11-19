'use client'

import { formatValue } from '@kyros/ui/charts/lib/format'
import type { Candle, ChartHover, ValueFormat } from '@kyros/ui/charts/lib/types'
import { MarketChart } from '@kyros/ui/charts/market-chart'
import { useLiveChartWindow } from '@kyros/ui/charts/use-live-chart-window'
import { useTweenedNumber } from '@kyros/ui/charts/use-tweened-number'
import { type ReactNode, useState } from 'react'

import { ChartShell } from './chart-shell'

export interface CandleChartProps {
  label: string
  title?: string
  meta?: ReactNode
  candles: readonly Candle[]
  /** Bucket length in milliseconds; each candle sits at its bucket on the time axis. */
  bucketMs: number
  /** Milliseconds of history to show. Defaults to every candle given. */
  windowMs?: number | undefined
  format: ValueFormat
  timeZone: string
  /** A horizontal reference such as the strike. */
  guide?: { value: number; label: string } | undefined
  /** The feed is streaming: the window rolls with the clock and the open candle grows. */
  live?: boolean
}

/** Roughly how long the open candle takes to reach a new high, low or close. */
const OPEN_CANDLE_SMOOTH_MS = 180

/**
 * The open candle's high, low and close glide to each tick instead of stepping, so the
 * one candle that is allowed to change does so smoothly; closed candles are never touched.
 */
function useSprungOpenCandle(candles: readonly Candle[], live: boolean): readonly Candle[] {
  const last = candles.at(-1)
  const options = { immediate: !live, smoothTime: OPEN_CANDLE_SMOOTH_MS }
  const high = useTweenedNumber(last?.high ?? 0, options)
  const low = useTweenedNumber(last?.low ?? 0, options)
  const close = useTweenedNumber(last?.close ?? 0, options)
  if (!last || !live) return candles
  return [
    ...candles.slice(0, -1),
    {
      ...last,
      high: Math.max(high, last.open, close),
      low: Math.min(low, last.open, close),
      close,
    },
  ]
}

/**
 * OHLC candles inside the chart shell, drawn by the market chart at compact density: each
 * candle is placed by its bucket time, so a new one appears at the right edge and the rest
 * scroll left rather than every candle narrowing by a slot.
 */
export function CandleChart({
  label,
  title,
  meta,
  candles,
  bucketMs,
  windowMs,
  format,
  timeZone,
  guide,
  live = false,
}: CandleChartProps) {
  const [hover, setHover] = useState<ChartHover | null>(null)
  const display = useSprungOpenCandle(candles, live)
  // The window is measured to each candle's close edge so the open candle is fully visible.
  const closes = candles.map(candle => ({ time: candle.time + bucketMs, value: candle.close }))
  const firstTime = candles[0]?.time
  const lastEdge = closes.at(-1)?.time
  const extent = firstTime !== undefined && lastEdge !== undefined ? lastEdge - firstTime : 60_000
  const { range } = useLiveChartWindow({ live, points: closes, spanMs: windowMs ?? extent })
  const shown = hover?.candle ?? candles.at(-1)
  const headerValue = meta ?? (shown ? formatValue(shown.close, format) : undefined)

  return (
    <ChartShell label={label} {...(title === undefined ? {} : { title })} value={headerValue}>
      {size => (
        <MarketChart
          kind="candlestick"
          density="compact"
          live={live}
          series={[]}
          candles={display}
          candleInterval={bucketMs}
          range={range}
          valueFormat={format}
          target={guide?.value}
          targetLabel={guide?.label}
          hideTickNear={guide?.value}
          timeZone={timeZone}
          title={label}
          emphasizeNowTick={live}
          onHoverChange={setHover}
          width={size.width}
          height={size.height}
        />
      )}
    </ChartShell>
  )
}
