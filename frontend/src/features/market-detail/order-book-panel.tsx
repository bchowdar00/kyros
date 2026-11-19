'use client'

import { formatExchangeSpread } from '@kyros/money'
import { IconButton } from '@kyros/ui/components/icon-button'
import { ScrollFade } from '@kyros/ui/components/scroll-fade'
import { SectionHeader } from '@kyros/ui/components/section-header'
import { CenterOrderBookIcon } from '@kyros/ui/icons/center-order-book-icon'
import { useLayoutEffect, useRef } from 'react'

import { GeminiBookSource } from './gemini/gemini-book-source'
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

/** One line in place of a side's levels, said only while the feed is live and the side is empty. */
function EmptySideRow({ children }: { children: string }) {
  return (
    <tr className="table w-full table-fixed">
      <td className="py-sm pl-sm text-left font-sans font-semibold text-foreground" colSpan={3}>
        {children}
      </td>
    </tr>
  )
}

export function OrderBookPanel({
  actionLabel,
  asks,
  bids,
  columnLabels,
  feedLive = false,
  geminiBook,
  spreadCents,
  spreadTicks,
  spreadLabel,
  spreadPercentage,
  title,
}: OrderBookPanelProps) {
  const scrollRegionRef = useRef<HTMLTableSectionElement>(null)
  const spreadRowRef = useRef<HTMLTableRowElement>(null)
  const followSpreadRef = useRef(true)
  // Only a series Gemini backs has an empty side that is Gemini's to name.
  const venue = geminiBook?.gemini === undefined ? '' : ' on Gemini'
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
          <div className="flex items-center gap-lg">
            {geminiBook === undefined ? null : <GeminiBookSource {...geminiBook} />}
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
          </div>
        }
        className="h-xl"
        title={title}
      />

      <div className="relative h-[438px] overflow-hidden border-y border-border-subtle bg-background">
        <table className="block h-full w-full table-fixed text-body font-medium text-foreground">
          <caption className="sr-only">{title}</caption>
          <thead className="relative block h-[53px] bg-background px-sm text-label font-semibold text-muted-foreground">
            <tr className="table h-[53px] w-full table-fixed border-b border-border-subtle">
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
            className="block h-[385px] overflow-y-auto py-lg font-mono overscroll-contain [overflow-anchor:none] [scrollbar-width:none] [&::-webkit-scrollbar]:hidden"
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
            {feedLive && asks.length === 0 ? (
              <EmptySideRow>{`No offers${venue} right now`}</EmptySideRow>
            ) : null}
            {asks.map(level => (
              <OrderBookRow
                depthPercentage={getDepthPercentage(level, maximumTotalUsd)}
                key={`ask-${'priceTicks' in level ? level.priceTicks : level.priceCents}`}
                level={level}
                side="away"
              />
            ))}

            <tr
              className="my-xs table h-3xl w-full table-fixed border-y border-border-subtle text-label text-muted-foreground"
              data-order-book-spread=""
              ref={spreadRowRef}
            >
              <th className="w-1/3 pl-sm text-left font-semibold" scope="row">
                {spreadLabel}
              </th>
              <td className="w-1/3 text-center font-bold text-foreground">
                {spreadTicks !== undefined
                  ? spreadTicks === null
                    ? '—'
                    : formatExchangeSpread(spreadTicks)
                  : `${spreadCents ?? '—'}¢`}
              </td>
              <td className="w-1/3 pr-sm text-right">
                {spreadPercentage === undefined
                  ? '—'
                  : percentageFormatter.format(spreadPercentage / 100)}
              </td>
            </tr>

            {feedLive && bids.length === 0 ? (
              <EmptySideRow>{`No bids${venue} right now`}</EmptySideRow>
            ) : null}
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
