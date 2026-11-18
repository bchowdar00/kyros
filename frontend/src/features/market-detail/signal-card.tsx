import { cn } from '@tiny/ui/lib/cn'

import type { MarketSide, MarketSignal } from './types'

export interface SignalCardProps {
  signal: MarketSignal
  /** Opens the number's detail view; without it the card is a static reading. */
  onOpen?: ((id: string, trigger: HTMLElement) => void) | undefined
}

const indicatorClasses: Record<MarketSide, string> = {
  away: 'border-market-away bg-market-away/80',
  home: 'border-market-home bg-market-home/80',
}

const cardClasses =
  'h-[96px] w-[162px] rounded-md border-[0.5px] border-border-subtle bg-surface-subtle p-xl text-left'

function CardBody({ signal }: { signal: MarketSignal }) {
  return (
    <dl className="flex h-full flex-col justify-between">
      <dt
        className="text-label font-medium tracking-label text-muted-foreground"
        title={signal.context}
      >
        {signal.label}
      </dt>
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
        {signal.value}
      </dd>
    </dl>
  )
}

export function SignalCard({ signal, onOpen }: SignalCardProps) {
  if (onOpen === undefined) {
    return (
      <article className={cardClasses}>
        <CardBody signal={signal} />
      </article>
    )
  }
  return (
    <button
      aria-label={`${signal.label}, ${signal.value}. Open details`}
      className={cn({
        [cardClasses]: true,
        'block cursor-pointer outline-none hover:bg-surface focus-visible:outline-2 focus-visible:outline-focus-ring': true,
      })}
      onClick={event => onOpen(signal.id, event.currentTarget)}
      type="button"
    >
      <CardBody signal={signal} />
    </button>
  )
}
