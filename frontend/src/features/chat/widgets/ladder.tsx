import type { ReactNode } from 'react'

import { ValueSlot } from './value-slot'
import { WidgetFrame } from '../widget-frame'

export interface LadderLevel {
  price: number
  size: number
}

export interface LadderProps {
  label: string
  title?: string
  meta?: ReactNode
  /** Best ask first. */
  asks: readonly LadderLevel[]
  /** Best bid first. */
  bids: readonly LadderLevel[]
  formatPrice: (price: number) => string
  formatSize: (size: number) => string
  /** Text on the rule between the two sides, usually the spread. */
  midLabel?: string
  rows?: number
}

const DEFAULT_ROWS = 7
/** Asks stack upward from the rule; the class swaps to `flex-col` for bids. */
const SIDE_CLASS =
  'flex max-h-ladder-side flex-col-reverse overflow-y-auto overscroll-contain rounded-xs focus-visible:outline-2 focus-visible:outline-focus-ring [scrollbar-width:thin]'

function LadderRow({
  level,
  side,
  share,
  formatPrice,
  formatSize,
}: {
  level: LadderLevel
  side: 'ask' | 'bid'
  share: number
  formatPrice: (price: number) => string
  formatSize: (size: number) => string
}) {
  return (
    <li className="relative flex justify-between px-sm py-xs font-mono text-label">
      <span
        aria-hidden="true"
        className={
          side === 'ask'
            ? 'absolute inset-y-0 right-0 rounded-xs bg-negative/15'
            : 'absolute inset-y-0 left-0 rounded-xs bg-positive/15'
        }
        style={{ width: `${share * 100}%` }}
      />
      <ValueSlot
        align="start"
        className={side === 'ask' ? 'relative text-negative' : 'relative text-positive'}
      >
        {formatPrice(level.price)}
      </ValueSlot>
      <ValueSlot className="relative text-foreground">{formatSize(level.size)}</ValueSlot>
    </li>
  )
}

/** An empty row keeping a side at its full height when the book is thinner than `rows`. */
function FillerRow() {
  return (
    <li aria-hidden="true" className="flex px-sm py-xs font-mono text-label">
      &nbsp;
    </li>
  )
}

/**
 * Two opposing depth series around a midpoint. Asks stack upward from the rule so the
 * best prices on each side meet in the middle, the way a trader reads a book.
 */
export function Ladder({
  label,
  title,
  meta,
  asks,
  bids,
  formatPrice,
  formatSize,
  midLabel,
  rows = DEFAULT_ROWS,
}: LadderProps) {
  const shownAsks = asks.slice(0, rows)
  const shownBids = bids.slice(0, rows)
  // Each side is always `rows` tall, so a level appearing or clearing never moves the reply.
  const askFillers = Array.from({ length: Math.max(0, rows - shownAsks.length) }, (_, i) => i)
  const bidFillers = Array.from({ length: Math.max(0, rows - shownBids.length) }, (_, i) => i)
  const largest = Math.max(
    1,
    ...shownAsks.map(level => level.size),
    ...shownBids.map(level => level.size)
  )

  return (
    <WidgetFrame label={label} {...(title === undefined ? {} : { title })} meta={meta}>
      <div className="flex justify-between px-sm text-caption text-muted-foreground">
        <span>Price</span>
        <span>Size</span>
      </div>
      <ol
        aria-label="Asks, best price last"
        className={SIDE_CLASS}
        // A deep book scrolls inside each side; scroll regions are keyboard reachable.
        // oxlint-disable-next-line jsx-a11y/no-noninteractive-tabindex
        tabIndex={0}
      >
        {shownAsks.map(level => (
          <LadderRow
            key={level.price}
            level={level}
            side="ask"
            share={level.size / largest}
            formatPrice={formatPrice}
            formatSize={formatSize}
          />
        ))}
        {askFillers.map(index => (
          <FillerRow key={`filler-${index}`} />
        ))}
      </ol>
      <div className="relative flex items-center">
        <span aria-hidden="true" className="h-px flex-1 bg-border" />
        {midLabel ? (
          <span className="px-sm font-mono text-caption text-muted-foreground">{midLabel}</span>
        ) : null}
        <span aria-hidden="true" className="h-px flex-1 bg-border" />
      </div>
      <ol
        aria-label="Bids, best price first"
        className={SIDE_CLASS.replace('flex-col-reverse', 'flex-col')}
        // oxlint-disable-next-line jsx-a11y/no-noninteractive-tabindex
        tabIndex={0}
      >
        {shownBids.map(level => (
          <LadderRow
            key={level.price}
            level={level}
            side="bid"
            share={level.size / largest}
            formatPrice={formatPrice}
            formatSize={formatSize}
          />
        ))}
        {bidFillers.map(index => (
          <FillerRow key={`filler-${index}`} />
        ))}
      </ol>
    </WidgetFrame>
  )
}
