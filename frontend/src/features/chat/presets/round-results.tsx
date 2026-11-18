import { MarketResultTriangleIcon } from '@tiny/ui/icons/market-result-triangle-icon'
import { cn } from '@tiny/ui/lib/cn'

import { WidgetFrame } from '../widget-frame'

export interface RoundResult {
  id: string
  outcome: 'up' | 'down'
  /** Signed move from strike at close, in dollars. */
  moveDollars: number
}

export interface RoundResultsProps {
  series: string
  /** Oldest first. */
  rounds: readonly RoundResult[]
}

function streak(rounds: readonly RoundResult[]) {
  const last = rounds.at(-1)
  if (!last) return 0
  let count = 0
  for (let index = rounds.length - 1; index >= 0; index -= 1) {
    if (rounds[index]?.outcome !== last.outcome) break
    count += 1
  }
  return count
}

/** The last rounds of a series as a strip of outcomes, oldest to newest, with the streak. */
export function RoundResults({ series, rounds }: RoundResultsProps) {
  const ups = rounds.filter(round => round.outcome === 'up').length
  const run = streak(rounds)
  const last = rounds.at(-1)
  const largest = Math.max(1, ...rounds.map(round => Math.abs(round.moveDollars)))

  return (
    <WidgetFrame
      label={`${series} recent round results`}
      title={`Last ${rounds.length} rounds`}
      meta={
        <span className="font-mono">
          {ups} up, {rounds.length - ups} down
        </span>
      }
    >
      <ol aria-label="Round outcomes, oldest first" className="flex h-4xl items-end gap-xs">
        {rounds.map(round => {
          const up = round.outcome === 'up'
          const height = 40 + (Math.abs(round.moveDollars) / largest) * 60
          return (
            <li
              key={round.id}
              aria-label={`${up ? 'Up' : 'Down'} ${Math.abs(round.moveDollars).toFixed(0)} dollars`}
              className="flex h-full flex-1 flex-col items-center justify-end gap-xs"
            >
              <span
                aria-hidden="true"
                className={cn('w-full rounded-xs', {
                  'bg-positive/30': up,
                  'bg-negative/30': !up,
                })}
                style={{ height: `${height}%` }}
              />
              <MarketResultTriangleIcon
                aria-hidden="true"
                className={cn('size-3', up ? 'rotate-180 text-positive' : 'text-negative')}
              />
            </li>
          )
        })}
      </ol>
      {last ? (
        <p className="text-label text-muted-foreground">
          {run > 1 ? `${run} in a row ${last.outcome}` : `Last round ${last.outcome}`}, the newest
          on the right. Bar height is the size of the move.
        </p>
      ) : null}
    </WidgetFrame>
  )
}
