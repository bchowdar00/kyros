'use client'

import type { RustSignalCatalogue } from '@kyros/domain'
import { MiniXIcon } from '@kyros/ui/icons/mini-x-icon'
import { NanoChevronIcon } from '@kyros/ui/icons/nano-chevron-icon'
import { cn } from '@kyros/ui/lib/cn'
import { useEffect, useId, useState } from 'react'

import {
  groupCardsByVenue,
  type SignalFieldCard,
  signalFieldCards,
  THIS_MARKET,
} from '@/features/signals/signal-cards'
import type { PinKey } from '@/features/signals/signal-catalogue'
import { SignalDetailDialog } from './signal-detail-dialog'
import { SignalRow } from './signal-row'
import type { SignalHistory } from '@/features/signals/signal-history'

export interface SignalsBrowserProps {
  catalogue: RustSignalCatalogue
  history: SignalHistory
  pins: readonly PinKey[]
  onTogglePin: (pin: PinKey) => void
  onClose: () => void
}

const NO_HISTORY: readonly never[] = []

function matches(card: SignalFieldCard, query: string): boolean {
  if (query === '') return true
  const haystack = [card.label, card.signalName, card.summary, card.venue].join(' ').toLowerCase()
  return query
    .toLowerCase()
    .split(/\s+/)
    .every(term => haystack.includes(term))
}

/** Every live value that applies to the market, one row per number, by venue and signal. */
export function SignalsBrowser({
  catalogue,
  history,
  pins,
  onTogglePin,
  onClose,
}: SignalsBrowserProps) {
  const [query, setQuery] = useState('')
  const [showAll, setShowAll] = useState(false)
  const [openVenues, setOpenVenues] = useState<ReadonlySet<string>>(() => new Set([THIS_MARKET]))
  // The open row by pin, so the detail view follows the live card rather than a snapshot.
  const [detail, setDetail] = useState<{ pin: PinKey; trigger: HTMLElement }>()
  const searchId = useId()

  const closeDetail = () => {
    detail?.trigger.focus()
    setDetail(undefined)
  }

  // Escape closes the detail view first, returning focus to its row, then the panel.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return
      if (detail === undefined) {
        onClose()
        return
      }
      detail.trigger.focus()
      setDetail(undefined)
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose, detail])

  const trimmed = query.trim()
  const cards = signalFieldCards(catalogue, { showAll })
  const groups = groupCardsByVenue(cards.filter(card => matches(card, trimmed)))
  const detailCard = detail === undefined ? undefined : cards.find(card => card.pin === detail.pin)

  const setVenueOpen = (venue: string, open: boolean) => {
    setOpenVenues(current => {
      if (current.has(venue) === open) return current
      const next = new Set(current)
      if (open) next.add(venue)
      else next.delete(venue)
      return next
    })
  }

  return (
    <dialog
      open
      aria-label="All signals"
      className="fixed inset-0 z-50 m-0 flex size-full max-h-none max-w-none items-stretch justify-end bg-background/60 p-0 text-foreground"
    >
      <button
        aria-label="Close signals"
        className="flex-1 cursor-default"
        onClick={onClose}
        type="button"
      />
      <section className="relative flex h-full w-full max-w-content flex-col gap-xl border-l-[0.5px] border-border-subtle bg-background p-3xl">
        <header className="flex items-center justify-between gap-lg">
          <div className="flex flex-col">
            <h2 className="text-heading-sm font-regular text-foreground">All signals</h2>
            <p className="text-label text-muted-foreground">
              {cards.length} live values for {catalogue.series}. Pinned rows show on the strip.
            </p>
          </div>
          <button
            aria-label="Close"
            className="min-h-touch min-w-touch rounded-xs p-sm text-muted-foreground outline-none focus-visible:outline-2 focus-visible:outline-focus-ring"
            onClick={onClose}
            type="button"
          >
            <MiniXIcon className="size-[18px]" />
          </button>
        </header>
        <div className="flex items-center gap-lg">
          <label className="flex min-w-0 flex-1 flex-col gap-xs" htmlFor={searchId}>
            <span className="sr-only">Search signals</span>
            <input
              autoComplete="off"
              className="min-h-touch rounded-sm border-[0.5px] border-transparent bg-surface-subtle px-lg text-body text-foreground outline-none placeholder:text-muted-foreground focus-visible:outline-2 focus-visible:outline-focus-ring"
              id={searchId}
              onChange={event => setQuery(event.target.value)}
              placeholder="Search signals"
              type="search"
              value={query}
            />
          </label>
          <button
            aria-pressed={showAll}
            className={cn({
              'min-h-touch shrink-0 rounded-sm border-[0.5px] bg-surface-subtle px-lg text-label outline-none hover:text-foreground focus-visible:outline-2 focus-visible:outline-focus-ring': true,
              'border-market-home text-foreground': showAll,
              'border-transparent text-muted-foreground': !showAll,
            })}
            onClick={() => setShowAll(current => !current)}
            type="button"
          >
            Show all fields
          </button>
        </div>
        <div className="flex min-h-0 flex-1 flex-col overflow-y-auto overscroll-contain [scrollbar-width:thin]">
          {groups.map(group => (
            <details
              className="group pt-2xl first:pt-0"
              key={group.venue}
              onToggle={event => setVenueOpen(group.venue, event.currentTarget.open)}
              open={trimmed !== '' || openVenues.has(group.venue)}
            >
              <summary className="sticky top-0 z-[1] flex min-h-touch cursor-pointer list-none items-center gap-lg border-b-[0.5px] border-border-subtle bg-background pb-md text-heading-md font-regular text-foreground outline-none focus-visible:outline-2 focus-visible:outline-focus-ring [&::-webkit-details-marker]:hidden">
                <span className="flex-1">{group.venue}</span>
                <span className="font-mono text-label text-muted-foreground">{group.count}</span>
                <NanoChevronIcon className="size-md -rotate-90 text-muted-foreground motion-safe:transition-transform group-open:rotate-0" />
              </summary>
              <div className="flex flex-col gap-lg pt-lg">
                {group.blocks.map(block => (
                  <section
                    aria-label={block.title}
                    className="flex flex-col gap-md rounded-sm bg-surface-subtle px-xl pt-lg pb-sm"
                    key={block.signalId}
                  >
                    <div className="flex flex-col gap-xs">
                      <h3 className="text-body font-medium text-foreground">{block.title}</h3>
                      <p className="text-label text-muted-foreground">{block.summary}</p>
                    </div>
                    <ul>
                      {block.cards.map(card => (
                        <li key={card.pin}>
                          <SignalRow
                            card={card}
                            history={history[card.pin] ?? NO_HISTORY}
                            onOpen={(opened, trigger) => setDetail({ pin: opened.pin, trigger })}
                            onToggle={onTogglePin}
                            pinned={pins.includes(card.pin)}
                          />
                        </li>
                      ))}
                    </ul>
                  </section>
                ))}
              </div>
            </details>
          ))}
          {groups.length === 0 ? (
            <p className="py-xl text-label text-muted-foreground">
              No live values match. Try another word, or show all fields.
            </p>
          ) : null}
        </div>
        {detailCard !== undefined ? (
          <SignalDetailDialog
            card={detailCard}
            history={history[detailCard.pin] ?? NO_HISTORY}
            onClose={closeDetail}
            onToggle={onTogglePin}
            pinned={pins.includes(detailCard.pin)}
          />
        ) : null}
      </section>
    </dialog>
  )
}
