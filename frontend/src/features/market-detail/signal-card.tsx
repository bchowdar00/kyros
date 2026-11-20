'use client'

import { cn } from '@kyros/ui/lib/cn'

import { useChangeCount } from '@/lib/use-change-count'

import { hasMovement, SignalSparkline } from './signal-sparkline'
import type { MarketSide, MarketSignal } from './types'

export interface SignalCardProps {
  signal: MarketSignal
  /** Opens the number's detail view; without it the card is a static reading. */
  onOpen?: ((id: string, trigger: HTMLElement) => void) | undefined
}

type Trend = 'down' | 'up'

const indicatorClasses: Record<MarketSide, string> = {
  away: 'border-market-away bg-market-away/80',
  home: 'border-market-home bg-market-home/80',
}

/** The light a card takes from the way its reading went over the last minute. */
const trendClasses: Record<Trend, string> = {
  down: 'border-highlight shadow-glow-inner-down',
  up: 'border-positive/60 shadow-glow-inner-up',
}

const cardClasses =
  'h-[96px] w-[162px] rounded-md border-[0.5px] border-border-subtle bg-surface-subtle px-xl py-lg text-left transition-[border-color,box-shadow,background-color] duration-300 ease-out-soft'

/** Which way a reading went over its trace; nothing when it did not move or is not followed. */
function trendOf(signal: MarketSignal): Trend | undefined {
  const { trace } = signal
  if (trace === undefined || !hasMovement(trace)) return undefined
  const first = trace[0]?.value
  const last = trace.at(-1)?.value
  if (first === undefined || last === undefined || first === last) return undefined
  return last > first ? 'up' : 'down'
}

/** The reading, lit for a moment when it changes and still when it arrives. */
function Value({ value }: { value: string }) {
  const changes = useChangeCount(value)

  return (
    <span
      className={cn('rounded-xs', changes > 0 && 'animate-tick')}
      // A new key restarts the light on every change.
      key={changes}
      {...(changes > 0 ? { 'data-ticked': '' } : {})}
    >
      {value}
    </span>
  )
}

function CardBody({ signal, trend }: { signal: MarketSignal; trend: Trend | undefined }) {
  return (
    <dl className="flex h-full flex-col justify-between">
      <dt
        // A long name keeps both its lines unless a trace needs the room.
        className={cn(
          'text-label font-medium tracking-label text-muted-foreground',
          trend === undefined ? 'line-clamp-2' : 'truncate'
        )}
        title={signal.context}
      >
        {signal.label}
      </dt>
      {trend !== undefined && signal.trace !== undefined ? (
        <SignalSparkline
          className="block h-[22px] w-full"
          colorVar={trend === 'down' ? '--color-negative' : '--color-positive'}
          points={signal.trace}
        />
      ) : null}
      <dd
        className="flex items-center gap-sm font-mono text-body font-regular text-foreground"
        data-signal-side={signal.side}
      >
        {signal.side ? (
          <span
            aria-hidden="true"
            className={`size-sm shrink-0 rounded-pill border-[0.5px] ${indicatorClasses[signal.side]}`}
          />
        ) : null}
        <Value value={signal.value} />
      </dd>
    </dl>
  )
}

export function SignalCard({ signal, onOpen }: SignalCardProps) {
  const trend = trendOf(signal)
  const classes = cn(cardClasses, trend !== undefined && trendClasses[trend])

  if (onOpen === undefined) {
    return (
      <article className={classes} data-trend={trend}>
        <CardBody signal={signal} trend={trend} />
      </article>
    )
  }
  return (
    <button
      aria-label={`${signal.label}, ${signal.value}. Open details`}
      className={cn(
        classes,
        'block cursor-pointer outline-none hover:bg-surface focus-visible:outline-2 focus-visible:outline-focus-ring'
      )}
      data-trend={trend}
      onClick={event => onOpen(signal.id, event.currentTarget)}
      type="button"
    >
      <CardBody signal={signal} trend={trend} />
    </button>
  )
}
