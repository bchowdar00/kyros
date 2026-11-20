'use client'

import { useState } from 'react'

import { cn } from '../lib/cn'
import { TimeMarker } from './chart-overlays'
import { formatValue } from './lib/format'
import { SECOND } from './lib/time'
import { impliedProbability } from './lib/tape'
import type { ChartFeed, ChartPoint } from './lib/types'
import { type ChartKind, type LineSeries, MarketChart } from './market-chart'
import { PriceChartTypeToggle, type PriceChartToggleKind } from './price-chart-type-toggle'
import { RecentMarketResults, type RecentMarketResult } from './recent-market-results'
import { type TradeMark, TradeMarks, valueOnLine } from './trade-marks'
import { useLiveChartWindow } from './use-live-chart-window'
import { ValueFlag } from './value-flag'
import { type VenueLine, VenueLines } from './venue-lines'
import { type ChartWindow, resolveDefaultChartWindow } from './chart-window'

const DEFAULT_SPAN = 5 * 60 * SECOND
// Keep the live view close enough to read individual moves, independent of market duration.
const LIVE_SPAN = 60 * SECOND
/** Share of the plot kept empty past the newest sample, so the line ends short of its flag. */
const TRAILING_SPACE = 0.12
/** Above this, cents are noise on the axis; below it they are the whole story. */
const WHOLE_UNIT_ABOVE = 100_000
const FLAG_FORMAT = {
  maximumFractionDigits: 2,
  minimumFractionDigits: 2,
  style: 'currency',
} as const

/** On the market's own line an order sits at the price paid for the side it bought. */
const pricePaid = (mark: TradeMark) => mark.percent

export interface PriceMarketCardProps {
  /** Symbol and line colour of the asset being charted. */
  asset: { color: string; symbol: string }
  /** Height of the plot, as a utility class. */
  chartClassName?: string
  className?: string
  /** Replay sources can advance from the supplied sample timestamps. */
  clock?: 'sample' | 'wall'
  /** Locks the card to one chart kind; otherwise the asset-price / probability switch shows. */
  fixedKind?: ChartKind
  /** Authoritative feed state for an application-owned live series. */
  feed?: ChartFeed | undefined
  points: readonly ChartPoint[]
  /** Absolute price distance the probability view maps onto 0-100. */
  priceBand: number
  /**
   * The market's own YES probability over the round, in percent. When supplied, the
   * probability view plots it and never the price-distance mapping; an empty list draws
   * nothing until the market prints a quote.
   */
  probabilityPoints?: readonly ChartPoint[] | undefined
  /** Resolved outcomes for the most recent price-against-target markets, oldest to newest. */
  recentMarketResults?: readonly RecentMarketResult[] | undefined
  /**
   * The open round. Price history runs continuously across rounds; samples before this
   * round's open draw dimmed behind an "Open" marker so the run-up reads as context.
   */
  round?: { opensAt: number } | undefined
  targetLabel: string
  targetPrice: number
  timeZone: string
  /** Rendered above the chart; omit where the page already has the market's name. */
  title?: string | undefined
  /** Orders placed on this market, drawn where they were placed. */
  tradeMarks?: readonly TradeMark[] | undefined
  /** Other venues' prices for YES, in percent; drawn on the market's line only. */
  venueLines?: readonly VenueLine[] | undefined
  /** Candidate data ranges; the card uses its primary range without exposing a chooser. */
  windows: readonly ChartWindow[]
}

export function PriceMarketCard({
  asset,
  chartClassName = 'h-chart-md',
  className,
  clock,
  fixedKind,
  feed,
  points,
  priceBand,
  probabilityPoints,
  recentMarketResults,
  round,
  targetLabel,
  targetPrice,
  timeZone,
  title,
  tradeMarks = [],
  venueLines = [],
  windows,
}: PriceMarketCardProps) {
  const [kind, setKind] = useState<ChartKind>(fixedKind ?? 'price')
  const window = resolveDefaultChartWindow(windows)
  const selectedSpan = window?.durationMs ?? DEFAULT_SPAN
  const span =
    feed === undefined || fixedKind === 'candlestick'
      ? selectedSpan
      : Math.min(selectedSpan, LIVE_SPAN)
  const sourceFeedState = feed?.state ?? 'paused'
  const live = sourceFeedState === 'live'
  const viewport = useLiveChartWindow({ clock: clock ?? 'wall', live, points, spanMs: span })

  const latestPrice = points.at(-1)?.value ?? Number.NaN
  // A market whose strike is not known yet charts price alone: no reference line, and
  // the probability view has nothing to compare against.
  const hasTarget = Number.isFinite(targetPrice)

  const valueFormat = {
    maximumFractionDigits: targetPrice >= WHOLE_UNIT_ABOVE ? 0 : 2,
    style: 'currency',
  } as const
  const priceSeries: LineSeries[] = [
    { color: asset.color, id: 'price', label: asset.symbol, points: points },
  ]
  // The market's own line when the exchange supplies one; the price-distance mapping is
  // only for sources with no market behind them (fixtures, the playground).
  const yesPoints =
    probabilityPoints ??
    (hasTarget
      ? points.map(point => ({
          time: point.time,
          value: impliedProbability(point.value, targetPrice, priceBand),
        }))
      : [])
  const probabilitySeries: LineSeries[] = [
    { color: 'var(--positive)', id: 'yes', label: 'Yes', points: yesPoints },
    {
      color: 'var(--negative)',
      id: 'no',
      label: 'No',
      points: yesPoints.map(point => ({
        time: point.time,
        value: Number((100 - point.value).toFixed(2)),
      })),
    },
  ]

  const handleKindChange = (value: PriceChartToggleKind) => {
    setKind(value)
  }

  return (
    <section
      aria-label={title ?? `${asset.symbol} price against ${targetLabel.toLowerCase()}`}
      className={cn('flex flex-col gap-lg', className)}
    >
      {title ? (
        <h2 className="text-heading-md font-regular text-foreground lg:text-2xl">{title}</h2>
      ) : null}

      <div className={cn('w-full', chartClassName)}>
        <MarketChart
          candleInterval={window?.candleInterval}
          dimBefore={round?.opensAt}
          emphasizeNowTick
          hideTickNear={
            kind === 'probability'
              ? undefined
              : [latestPrice, targetPrice].filter(value => Number.isFinite(value))
          }
          kind={kind}
          live={live}
          overlays={
            kind === 'probability' ? null : (
              <ValueFlag
                color={asset.color}
                label={formatValue(latestPrice, FLAG_FORMAT)}
                value={latestPrice}
                seriesId="price"
              />
            )
          }
          range={viewport.range}
          series={kind === 'probability' ? probabilitySeries : priceSeries}
          target={kind === 'probability' || !hasTarget ? undefined : targetPrice}
          targetLabel={targetLabel}
          timeZone={timeZone}
          title={`${title ?? asset.symbol} ${kind} chart`}
          trailingSpace={TRAILING_SPACE}
          valueFormat={kind === 'probability' ? undefined : valueFormat}
        >
          {round ? <TimeMarker label="Open" time={round.opensAt} /> : null}
          {kind === 'probability' ? <VenueLines lines={venueLines} /> : null}
          {/* On the price line an order sits where the price was when it was placed. */}
          <TradeMarks
            marks={tradeMarks}
            valueOf={kind === 'probability' ? pricePaid : mark => valueOnLine(points, mark.time)}
          />
        </MarketChart>
      </div>

      <div className="flex items-center justify-between gap-lg">
        <RecentMarketResults results={recentMarketResults} />

        {fixedKind ? null : (
          <div className="ml-auto">
            <PriceChartTypeToggle
              assetSymbol={asset.symbol}
              onChange={handleKindChange}
              value={kind === 'probability' ? 'probability' : 'price'}
            />
          </div>
        )}
      </div>
    </section>
  )
}
