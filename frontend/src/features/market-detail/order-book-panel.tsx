'use client'

import { formatExchangeSpread } from '@tiny/money'
import { IconButton } from '@tiny/ui/components/icon-button'
import { ScrollFade } from '@tiny/ui/components/scroll-fade'
import { SectionHeader } from '@tiny/ui/components/section-header'
import { CenterOrderBookIcon } from '@tiny/ui/icons/center-order-book-icon'
import { useLayoutEffect, useRef } from 'react'

import { OrderBookRow } from './order-book-row'
import type { OrderBookData, OrderBookLevel } from './types'

export type OrderBookPanelProps = OrderBookData

const percentageFormatter = new Intl.NumberFormat('en-US', {
  maximumFractionDigits: 2,
  minimumFractionDigits: 2,
  style: 'percent',
})

function getDepthPercentage(level: OrderBookLevel, maximumTotalUsd: number) {
  if ('priceTicks' in level) return level.depthPercentage
  if (maximumTotalUsd <= 0) {
    return 0
  }

  return Math.min(100, Math.max(4, (level.totalUsd / maximumTotalUsd) * 100))
}

function centerSpread(
  scrollRegion: HTMLTableSectionElement | null,
  spreadRow: HTMLTableRowElement | null
) {
  if (!scrollRegion || !spreadRow) {
    return
  }

  const spreadTopWithinRegion =
    spreadRow.getBoundingClientRect().top -
    scrollRegion.getBoundingClientRect().top +
    scrollRegion.scrollTop
  const top = spreadTopWithinRegion - (scrollRegion.clientHeight - spreadRow.offsetHeight) / 2

  scrollRegion.scrollTop = top
}

export function OrderBookPanel({
  actionLabel,
  asks,
  bids,
  columnLabels,
  spreadCents,
  spreadTicks,
  spreadLabel,
  spreadPercentage,
  title,
}: OrderBookPanelProps) {
  const scrollRegionRef = useRef<HTMLTableSectionElement>(null)
  const spreadRowRef = useRef<HTMLTableRowElement>(null)
  const followSpreadRef = useRef(true)
  const maximumTotalUsd = Math.max(
    0,
    ...asks.map(level => ('priceTicks' in level ? 0 : level.totalUsd)),
    ...bids.map(level => ('priceTicks' in level ? 0 : level.totalUsd))
  )

  useLayoutEffect(() => {
    if (followSpreadRef.current) centerSpread(scrollRegionRef.current, spreadRowRef.current)
  })

  return (
    <section aria-label={title} className="flex w-full flex-col gap-xl">
      <SectionHeader
        action={
          <IconButton
            aria-label={actionLabel}
            className="size-2xl border-0 p-0 text-foreground"
            onClick={() => {
              followSpreadRef.current = true
              centerSpread(scrollRegionRef.current, spreadRowRef.current)
            }}
            size="small"
            variant="ghost"
          >
            <CenterOrderBookIcon />
          </IconButton>
        }
        className="h-xl"
        title={title}
      />

      <div className="relative h-[438px] overflow-hidden rounded-md border border-highlight bg-background">
        <table className="block h-full w-full table-fixed text-label font-medium text-foreground">
          <caption className="sr-only">{title}</caption>
          <thead className="relative block h-[53px] bg-background/50 px-xl text-caption font-semibold uppercase text-muted-foreground backdrop-blur-glass">
            <tr className="table h-[53px] w-full table-fixed border-b border-highlight">
              <th className="w-1/3 text-left font-semibold" scope="col">
                {columnLabels[0]}
              </th>
              <th className="w-1/3 text-center font-semibold" scope="col">
                {columnLabels[1]}
              </th>
              <th className="w-1/3 text-right font-semibold" scope="col">
                {columnLabels[2]}
              </th>
            </tr>
          </thead>

          {/* The scrollable row group needs keyboard focus and scroll intent handlers. */}
          {/* oxlint-disable-next-line jsx-a11y/no-noninteractive-element-interactions */}
          <tbody
            className="block h-[385px] overflow-y-auto px-xl py-lg font-mono overscroll-contain [overflow-anchor:none] [scrollbar-width:none] [&::-webkit-scrollbar]:hidden"
            data-order-book-scroll=""
            ref={scrollRegionRef}
            // oxlint-disable-next-line jsx-a11y/no-noninteractive-tabindex
            tabIndex={0}
            aria-label="Order book levels"
            onWheel={() => {
              followSpreadRef.current = false
            }}
            onTouchMove={() => {
              followSpreadRef.current = false
            }}
            onKeyDown={event => {
              if (
                ['ArrowUp', 'ArrowDown', 'PageUp', 'PageDown', 'Home', 'End', ' '].includes(
                  event.key
                )
              )
                followSpreadRef.current = false
            }}
          >
            {asks.map(level => (
              <OrderBookRow
                depthPercentage={getDepthPercentage(level, maximumTotalUsd)}
                key={`ask-${'priceTicks' in level ? level.priceTicks : level.priceCents}`}
                level={level}
                side="away"
              />
            ))}

            <tr
              className="mb-xs table h-[28px] w-full table-fixed"
              data-order-book-spread=""
              ref={spreadRowRef}
            >
              <th className="relative w-1/3 text-right font-medium" scope="row">
                <span
                  aria-hidden="true"
                  className="absolute inset-y-0 left-0 z-0 w-[300%] rounded-xs border border-border-subtle bg-surface-raised"
                />
                <span className="relative z-[1]">{spreadLabel}</span>
              </th>
              <td className="relative z-[1] w-1/3 text-center">
                {spreadTicks !== undefined
                  ? spreadTicks === null
                    ? '—'
                    : formatExchangeSpread(spreadTicks)
                  : `${spreadCents ?? '—'}¢`}
              </td>
              <td className="relative z-[1] w-1/3 text-left">
                {spreadPercentage === undefined
                  ? '—'
                  : percentageFormatter.format(spreadPercentage / 100)}
              </td>
            </tr>

            {bids.map(level => (
              <OrderBookRow
                depthPercentage={getDepthPercentage(level, maximumTotalUsd)}
                key={`bid-${'priceTicks' in level ? level.priceTicks : level.priceCents}`}
                level={level}
                side="home"
              />
            ))}
          </tbody>
        </table>

        <ScrollFade className="top-[53px] h-lg from-background/85" edge="top" />
        <ScrollFade className="h-lg from-background/85" edge="bottom" />
      </div>
    </section>
  )
}
