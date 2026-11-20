'use client'

import { useChartGeometry } from './internal/chart-context'
import type { ChartPoint } from './lib/types'

/** One order a strategy or a person placed, as the chart shows it. */
export interface TradeMark {
  id: string
  /** What the order was, in words: read aloud, and shown when the mark is pointed at. */
  label: string
  /** The price paid, in percent, for charts whose axis is a probability. */
  percent?: number
  /** Which outcome was bought: the first points up, the second points down. */
  side: 'first' | 'second'
  /** When it was placed, unix ms. */
  time: number
}

export interface TradeMarksProps {
  marks: readonly TradeMark[]
  /** Where a mark sits on the value axis; a mark with nowhere to sit is not drawn. */
  valueOf: (mark: TradeMark) => number | undefined
}

/** Marks closer together than this, on the same side, are drawn as one. */
const MERGE_PX = 10
/** Half the width of a mark, and how far its tip stands off the value it marks. */
const HALF = 6
const GAP = 5

interface Placed {
  count: number
  /** The first order of the group: the mark's identity while the chart moves under it. */
  id: string
  label: string
  side: TradeMark['side']
  x: number
  y: number
}

/** The value of a line at a moment between its samples; nothing outside the line. */
export function valueOnLine(points: readonly ChartPoint[], time: number): number | undefined {
  const after = points.findIndex(point => point.time >= time)
  const next = points[after]
  if (next === undefined) return undefined
  if (next.time === time) return next.value
  const previous = points[after - 1]
  if (previous === undefined) return undefined
  const share = (time - previous.time) / (next.time - previous.time)
  return previous.value + (next.value - previous.value) * share
}

function named(placed: Placed): string {
  if (placed.count === 1) return placed.label
  const more = placed.count - 1
  return `${placed.label}, and ${more} more ${more === 1 ? 'order' : 'orders'}`
}

/** A triangle whose tip stands just off (x, y): below it pointing up, or above it pointing down. */
function outline(placed: Placed): string {
  const { x, y } = placed
  if (placed.side === 'first') {
    const tip = y + GAP
    return `${x},${tip} ${x - HALF},${tip + 2 * HALF} ${x + HALF},${tip + 2 * HALF}`
  }
  const tip = y - GAP
  return `${x},${tip} ${x - HALF},${tip - 2 * HALF} ${x + HALF},${tip - 2 * HALF}`
}

/**
 * Where orders were placed, on the chart they were placed against: a small
 * triangle per order, up for the first outcome and down for the second.
 * Orders placed almost together on one side are one mark that says how many.
 */
export function TradeMarks({ marks, valueOf }: TradeMarksProps) {
  const { plot, xScale, yScale } = useChartGeometry()

  const placed: Placed[] = []
  for (const mark of marks.toSorted((a, b) => a.time - b.time)) {
    const value = valueOf(mark)
    if (value === undefined) continue
    const x = xScale(mark.time)
    const y = yScale(value)
    if (!Number.isFinite(y) || x < plot.left || x > plot.right) continue
    const near = placed.findLast(each => each.side === mark.side)
    if (near !== undefined && x - near.x < MERGE_PX) {
      near.count += 1
      continue
    }
    placed.push({
      count: 1,
      id: mark.id,
      label: mark.label,
      side: mark.side,
      x,
      y: Math.min(Math.max(y, plot.top), plot.bottom),
    })
  }

  return (
    <g data-chart-trade-marks="">
      {placed.map(each => (
        <g
          aria-label={named(each)}
          data-trade-mark={each.side}
          data-x={each.x}
          key={each.id}
          // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- an SVG group has no native image element
          role="img"
        >
          <title>{named(each)}</title>
          <polygon
            className={
              each.side === 'first'
                ? 'fill-positive stroke-background'
                : 'fill-negative stroke-background'
            }
            points={outline(each)}
            strokeLinejoin="round"
            strokeWidth={1.5}
          />
        </g>
      ))}
    </g>
  )
}
