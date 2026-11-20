'use client'

import { useState } from 'react'

import { cn } from '../lib/cn'
import { TimeMarker } from './chart-overlays'
import { formatValue } from './lib/format'
import type { ChartPoint, TimeRange } from './lib/types'
import { type ChartHover, type LineSeries, MarketChart } from './market-chart'
import { type TradeMark, TradeMarks } from './trade-marks'
import { useTweenedNumber } from './use-tweened-number'
import { type VenueLine, VenueLines } from './venue-lines'

const PERCENT = { style: 'percent' } as const

/** Both lines are prices, so an order sits at the price paid for the side it bought. */
const pricePaid = (mark: TradeMark) => mark.percent

export interface ProbabilityOutcome {
  color: string
  id: string
  label: string
}

export interface ProbabilityMarketCardProps {
  /** Height of the plot, as a utility class. */
  chartClassName?: string
  className?: string
  /** Vertical guides such as period boundaries. */
  markers?: readonly { label: string; time: number }[]
  /**
   * The two sides of the market. The first carries the samples; the second is
   * its complement, because two outcomes always sum to 100.
   */
  outcomes: readonly [ProbabilityOutcome & { points: readonly ChartPoint[] }, ProbabilityOutcome]
  range: TimeRange
  subtitle?: string | undefined
  /** Whether the supplied observations are from a live feed. */
  live?: boolean
  timeZone: string
  /** Rendered above the readout; omit where the page already has the market's name. */
  title?: string | undefined
  /** Orders placed on this market, drawn where they were placed. */
  tradeMarks?: readonly TradeMark[] | undefined
  /** Other venues' prices for the first outcome, in percent, as quiet lines. */
  venueLines?: readonly VenueLine[] | undefined
}

/**
 * A two-outcome market over a fixed window: the two live probabilities and the chart.
 * Every screen that shows one renders this.
 */
export function ProbabilityMarketCard({
  chartClassName = 'h-chart-sm',
  className,
  markers = [],
  outcomes,
  range,
  subtitle,
  live = false,
  timeZone,
  title,
  tradeMarks = [],
  venueLines = [],
}: ProbabilityMarketCardProps) {
  const [primary, complement] = outcomes
  const [hover, setHover] = useState<ChartHover | null>(null)
  const series: LineSeries[] = [
    { color: primary.color, id: primary.id, label: primary.label, points: primary.points },
    {
      color: complement.color,
      id: complement.id,
      label: complement.label,
      points: primary.points.map(point => ({
        time: point.time,
        value: Number((100 - point.value).toFixed(2)),
      })),
    },
  ]

  const outcomeValue = (seriesId: string) => {
    const item = series.find(candidate => candidate.id === seriesId)
    const hovered = hover?.values.find(value => value.seriesId === seriesId)?.point.value

    return hovered ?? item?.points.at(-1)?.value ?? 0
  }
  // Scrubbing snaps to the sample; live ticks glide so the readout reads as one motion.
  const isHovering = hover !== null
  const primaryValue = useTweenedNumber(outcomeValue(primary.id), { immediate: isHovering })
  const complementValue = useTweenedNumber(outcomeValue(complement.id), { immediate: isHovering })
  const readout = [
    { color: primary.color, id: primary.id, value: primaryValue },
    { color: complement.color, id: complement.id, value: complementValue },
  ]
  const summary = series
    .map(item => `${item.label} ${formatValue(item.points.at(-1)?.value ?? 0, PERCENT)}`)
    .join(', ')

  return (
    <section
      aria-label={title ?? 'Market probability'}
      className={cn('flex flex-col gap-2xl', className)}
    >
      <header className="flex flex-wrap items-end justify-between gap-lg">
        <div className="min-w-0">
          {title ? (
            <h2 className="text-heading-md font-regular text-foreground lg:text-2xl">{title}</h2>
          ) : null}
          {subtitle ? (
            <p className="mt-xs text-body font-medium text-muted-foreground">{subtitle}</p>
          ) : null}
        </div>

        <div className="flex shrink-0 flex-wrap items-center gap-lg lg:gap-2xl">
          {/* text-2xl (24px) is large text under WCAG, where the market colours meet the
              3:1 contrast floor on black; below that size the away green falls short. */}
          <p className="flex items-center gap-lg text-2xl font-regular font-mono">
            {readout.map(item => (
              <span key={item.id} style={{ color: item.color }}>
                {formatValue(item.value, PERCENT)}
              </span>
            ))}
          </p>
        </div>
      </header>

      <div className={cn('w-full', chartClassName)}>
        <MarketChart
          emphasizeNowTick
          kind="probability"
          live={live}
          onHoverChange={setHover}
          // The scheduled window stays the right edge: a game in progress shows the
          // time it has left rather than ending at the newest tick.
          range={range}
          series={series}
          timeZone={timeZone}
          title={`${title ? `${title} ` : ''}probability over time: ${summary}`}
        >
          {markers.map(marker => (
            <TimeMarker key={marker.time} label={marker.label} time={marker.time} />
          ))}
          <VenueLines lines={venueLines} />
          <TradeMarks marks={tradeMarks} valueOf={pricePaid} />
        </MarketChart>
      </div>
    </section>
  )
}
