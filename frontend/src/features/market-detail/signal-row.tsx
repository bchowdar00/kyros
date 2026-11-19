'use client'

import type { ChartPoint } from '@kyros/ui/charts/lib/types'
import { cn } from '@kyros/ui/lib/cn'

import type { SignalFieldCard } from '@/features/signals/signal-cards'
import type { PinKey } from '@/features/signals/signal-catalogue'
import { hasMovement, SignalSparkline } from './signal-sparkline'

export interface SignalRowProps {
  card: SignalFieldCard
  /** Recent samples for a number; ignored for a state. */
  history: readonly ChartPoint[]
  pinned: boolean
  onToggle: (pin: PinKey) => void
  /** Opens the detail view; receives the row so focus can return to it. */
  onOpen?: (card: SignalFieldCard, trigger: HTMLElement) => void
}

/** One number on the ledger: its name, its recent trace, its value, and a pin toggle. */
export function SignalRow({ card, history, pinned, onToggle, onOpen }: SignalRowProps) {
  return (
    <div className="group/row -mx-md flex min-h-touch items-center gap-md rounded-xs hover:bg-surface">
      <button
        className="flex min-h-touch min-w-0 flex-1 cursor-pointer items-center gap-lg rounded-xs px-md py-sm text-left outline-none focus-visible:outline-2 focus-visible:outline-focus-ring"
        onClick={event => onOpen?.(card, event.currentTarget)}
        title={card.summary}
        type="button"
      >
        <span className="min-w-0 flex-1 truncate text-body text-muted-foreground group-hover/row:text-foreground">
          {card.label}
        </span>
        <span className="w-signal-trace shrink-0">
          {card.kind === 'number' && hasMovement(history) ? (
            <SignalSparkline points={history} />
          ) : null}
        </span>
        <span className="w-signal-value shrink-0 text-right font-mono text-body text-foreground">
          {card.value}
        </span>
      </button>
      <button
        aria-label={`${pinned ? 'Unpin' : 'Pin'} ${card.signalName} ${card.label}`}
        aria-pressed={pinned}
        className={cn({
          'min-h-touch min-w-touch shrink-0 rounded-xs px-md text-label outline-none hover:bg-surface-raised focus-visible:outline-2 focus-visible:outline-focus-ring': true,
          'text-market-home': pinned,
          'text-muted-foreground hover:text-foreground': !pinned,
        })}
        onClick={() => onToggle(card.pin)}
        type="button"
      >
        {pinned ? 'Pinned' : 'Pin'}
      </button>
    </div>
  )
}
