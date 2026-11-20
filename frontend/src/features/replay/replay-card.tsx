'use client'

import { AnimatedNumber } from '@kyros/ui/components/animated-number'
import { cn } from '@kyros/ui/lib/cn'
import Link from 'next/link'

import { type ReplayIndexEntry, replayDay } from './replay'
import { useReplay } from './use-replay'

const focusRing =
  'focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus-ring'
const panel = 'rounded-md border border-border-subtle bg-background/70'

/** A score that rolls to its new value; read aloud as a plain number. */
function Score({ value }: { value: number }) {
  return (
    <>
      <span className="sr-only">{value}</span>
      <span aria-hidden="true">
        <AnimatedNumber locales="en-US" value={value} />
      </span>
    </>
  )
}

function signed(change: number): string {
  return `${change >= 0 ? '+' : ''}${change.toFixed(1)}`
}

function changeTone(change: number): string | false {
  return (change > 0 && 'text-positive') || (change < 0 && 'text-negative')
}

/** One finished game, replaying on the home grid, wholly linked to its replay page. */
export function ReplayCard({ entry, index }: { entry: ReplayIndexEntry; index: number }) {
  const { failed, position, state } = useReplay(entry, index)

  return (
    <article
      className={cn(
        panel,
        'relative flex animate-rise flex-col gap-md p-lg transition-[border-color] duration-200 ease-out-soft hover:border-highlight'
      )}
    >
      <Link
        className={cn('absolute inset-0 rounded-md', focusRing)}
        href={{ pathname: `/game/${entry.id}` }}
      >
        <span className="sr-only">
          {entry.away.name} at {entry.home.name}
        </span>
      </Link>

      <span className="text-label text-muted-foreground">NBA · {replayDay(entry.date)}</span>

      {state === undefined ? (
        <>
          <div className="flex flex-col gap-2xs font-mono text-body text-foreground">
            <span>{entry.away.code}</span>
            <span>{entry.home.code}</span>
          </div>
          <span className="text-caption text-muted-foreground">
            {failed ? 'Game unavailable' : 'Loading game'}
          </span>
        </>
      ) : (
        <>
          <dl className="flex flex-col gap-2xs">
            <div className="flex items-baseline justify-between gap-md">
              <dt className="font-mono text-body text-muted-foreground">{entry.away.code}</dt>
              <dd className="font-mono text-heading-sm font-bold text-foreground">
                <Score value={state.away} />
              </dd>
            </div>
            <div className="flex items-baseline justify-between gap-md">
              <dt className="font-mono text-body text-muted-foreground">{entry.home.code}</dt>
              <dd className="font-mono text-heading-sm font-bold text-foreground">
                <Score value={state.home} />
              </dd>
            </div>
          </dl>

          <div className="flex items-baseline justify-between gap-md text-caption">
            <span className="text-muted-foreground">
              {position.ended ? 'Final' : `Q${state.period} ${state.clock}`}
            </span>
            {state.homePercent === undefined ? null : (
              <span className="flex items-baseline gap-xs font-mono">
                <span className="text-muted-foreground">
                  {entry.home.code} {state.homePercent}%
                </span>
                {state.changePercent === undefined ? null : (
                  <span className={cn(changeTone(state.changePercent))}>
                    {signed(state.changePercent)}
                  </span>
                )}
              </span>
            )}
          </div>
        </>
      )}
    </article>
  )
}
