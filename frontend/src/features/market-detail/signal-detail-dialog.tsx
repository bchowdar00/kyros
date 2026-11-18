'use client'

import type { ChartPoint } from '@tiny/ui/charts/lib/types'
import { MiniXIcon } from '@tiny/ui/icons/mini-x-icon'
import { cn } from '@tiny/ui/lib/cn'
import { useEffect, useId, useRef } from 'react'

import type { SignalFieldCard } from '@/features/signals/signal-cards'
import type { PinKey } from '@/features/signals/signal-catalogue'
import { hasMovement, SignalSparkline } from './signal-sparkline'

export interface SignalDetailDialogProps {
  card: SignalFieldCard
  history: readonly ChartPoint[]
  pinned: boolean
  onToggle: (pin: PinKey) => void
  onClose: () => void
  /** Fill the nearest positioned ancestor (the panel), or the whole viewport. */
  placement?: 'panel' | 'viewport'
}

/** One number explained: what it is, what its signal is, and its last minute, with a pin. */
export function SignalDetailDialog({
  card,
  history,
  pinned,
  onToggle,
  onClose,
  placement = 'panel',
}: SignalDetailDialogProps) {
  const titleId = useId()
  const closeRef = useRef<HTMLButtonElement>(null)
  const traced = card.kind === 'number' && hasMovement(history)

  // Focus lands on the dialog when it opens; the opener returns it on close.
  useEffect(() => {
    closeRef.current?.focus()
  }, [])

  return (
    <dialog
      open
      aria-labelledby={titleId}
      className={cn({
        'inset-0 m-0 flex size-full max-h-none max-w-none items-center justify-center bg-background/70 p-3xl text-foreground': true,
        'absolute z-[2]': placement === 'panel',
        'fixed z-50': placement === 'viewport',
      })}
    >
      <button
        aria-label="Close details"
        className="absolute inset-0 cursor-default"
        onClick={onClose}
        type="button"
      />
      <article className="relative flex w-full max-w-content-copy flex-col gap-2xl rounded-md border-[0.5px] border-border-subtle bg-background p-3xl">
        <header className="flex items-start justify-between gap-lg">
          <div className="flex flex-col gap-xs">
            <p className="text-label text-muted-foreground">
              {card.signalName} on {card.venue}
            </p>
            <h2 className="text-heading-sm font-regular text-foreground" id={titleId}>
              {card.label}
            </h2>
          </div>
          <button
            aria-label={`Close ${card.label}`}
            className="min-h-touch min-w-touch rounded-xs p-sm text-muted-foreground outline-none focus-visible:outline-2 focus-visible:outline-focus-ring"
            onClick={onClose}
            ref={closeRef}
            type="button"
          >
            <MiniXIcon className="size-[18px]" />
          </button>
        </header>

        <div className="flex flex-col gap-md">
          <p className="font-mono text-heading-md text-foreground">{card.value}</p>
          {traced ? (
            <SignalSparkline className="block h-5xl w-full" points={history} />
          ) : (
            <p className="text-label text-muted-foreground">
              {card.kind === 'number'
                ? 'The trace draws once this value has moved.'
                : 'A state, not a number, so there is no trace.'}
            </p>
          )}
        </div>

        <div className="flex flex-col gap-lg">
          <p className="text-body text-foreground">{card.description ?? card.summary}</p>
          {card.description !== undefined ? (
            <p className="text-label text-muted-foreground">{card.summary}</p>
          ) : null}
        </div>

        <footer className="flex items-center justify-end gap-lg">
          <button
            aria-pressed={pinned}
            className={cn({
              'min-h-touch rounded-sm border-[0.5px] px-xl text-body outline-none focus-visible:outline-2 focus-visible:outline-focus-ring': true,
              'border-market-home text-foreground': pinned,
              'border-border-subtle text-muted-foreground': !pinned,
            })}
            onClick={() => onToggle(card.pin)}
            type="button"
          >
            {pinned ? 'Pinned to strip' : 'Pin to strip'}
          </button>
        </footer>
      </article>
    </dialog>
  )
}
