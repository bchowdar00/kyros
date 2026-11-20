'use client'

import { useChartGeometry } from './internal/chart-context'
import type { ChartPoint } from './lib/types'

/** Another venue's price for the same market, in the chart's own units. */
export interface VenueLine {
  id: string
  /** The venue's name, written once at the end of its line. */
  label: string
  points: readonly ChartPoint[]
}

/** How far a label stands off the end of its line. */
const LABEL_OFFSET = 8
/** Room a label needs to the left of the line's end; with less, it is written to the right. */
const LABEL_ROOM = 120
/** Written to the right it stands further off, clear of the market's own value at the same spot. */
const CLEAR_OF_VALUE = 22

/**
 * Other venues' prices as thin dotted lines behind the market's own, each
 * named once at its newest reading. They are context, so they carry no
 * colour of their own and never take the pointer.
 */
export function VenueLines({ lines }: { lines: readonly VenueLine[] }) {
  const { plot, xScale, yScale } = useChartGeometry()

  return (
    <>
      {lines.map((line, index) => {
        const last = line.points.at(-1)
        if (last === undefined || line.points.length < 2) return null
        const x = Math.min(xScale(last.time), plot.right)
        const y = yScale(last.value)
        // Alternate sides of the line so two venues at one price stay readable.
        const above = index % 2 === 0
        const roomOnLeft = x - plot.left >= LABEL_ROOM
        const lift = roomOnLeft ? LABEL_OFFSET : CLEAR_OF_VALUE
        return (
          <g data-venue-line={line.id} key={line.id} pointerEvents="none">
            <polyline
              className="stroke-muted-foreground"
              fill="none"
              opacity={0.75}
              points={line.points
                .map(point => `${xScale(point.time)},${yScale(point.value)}`)
                .join(' ')}
              strokeDasharray="2 3"
              strokeLinecap="round"
              strokeWidth={1.25}
            />
            <text
              className="fill-muted-foreground text-caption"
              dominantBaseline={above ? 'auto' : 'hanging'}
              textAnchor={roomOnLeft ? 'end' : 'start'}
              x={roomOnLeft ? x - LABEL_OFFSET : x + LABEL_OFFSET}
              y={above ? y - lift : y + lift}
            >
              {`${line.label} ${Math.round(last.value)}%`}
            </text>
          </g>
        )
      })}
    </>
  )
}
