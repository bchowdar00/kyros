'use client'

import { Button } from '@kyros/ui/components/button'
import { cn } from '@kyros/ui/lib/cn'
import Link from 'next/link'
import { useState } from 'react'

import { MarketCountdown } from '@/features/market-detail/market-countdown'
import { useChangeCount } from '@/lib/use-change-count'

import type { GameCard, QuestionCard, UpDownCard } from './gemini-board'
import type { GeminiBoard } from './gemini-board-loader'

type BoardCard = GameCard | QuestionCard | UpDownCard

const ALL = 'All'
const CRYPTO = 'Crypto'
/** Cards shown before "Show more": twelve rows of four. */
const PAGE = 48
/** Cards that come in one after another; the rest arrive with the last of them. */
const STAGGERED = 8
const STAGGER_MS = 30

const readAt = new Intl.DateTimeFormat('en-US', {
  hour: '2-digit',
  hourCycle: 'h23',
  minute: '2-digit',
  timeZone: 'UTC',
})

const startsAt = new Intl.DateTimeFormat('en-US', {
  day: 'numeric',
  hour: 'numeric',
  minute: '2-digit',
  month: 'short',
})

const closesOn = new Intl.DateTimeFormat('en-US', { day: 'numeric', month: 'short' })

/** The Kyros market a card trades, when it has one; question cards never do. */
function tradeSlugOf(card: BoardCard): string | undefined {
  return card.kind === 'question' ? undefined : card.tradeSlug
}

function groupOf(card: BoardCard): string {
  if (card.kind === 'updown') return CRYPTO
  if (card.kind === 'question') return card.category
  return card.league
}

/** The filters: everything, then the up or down rounds, then each league, the fullest first. */
function filtersOf(cards: readonly BoardCard[]): { count: number; name: string }[] {
  const counts = new Map<string, number>()
  for (const card of cards) counts.set(groupOf(card), (counts.get(groupOf(card)) ?? 0) + 1)
  const groups = [...counts]
    .map(([name, count]) => ({ count, name }))
    // The up or down rounds lead: they are what Kyros trades most of.
    .toSorted(
      (a, b) =>
        Number(b.name === CRYPTO) - Number(a.name === CRYPTO) ||
        b.count - a.count ||
        a.name.localeCompare(b.name)
    )
  return [{ count: cards.length, name: ALL }, ...groups]
}

function sidesOf(card: BoardCard): readonly { cents?: number; label: string }[] {
  if (card.kind === 'game' || card.kind === 'question') return card.outcomes
  return [
    { ...(card.upCents === undefined ? {} : { cents: card.upCents }), label: 'Up' },
    { ...(card.downCents === undefined ? {} : { cents: card.downCents }), label: 'Down' },
  ]
}

/**
 * A price that lights for a moment when it changes, so a number that moved is seen to move.
 * Nothing lights on arrival.
 */
function Price({ cents }: { cents: number }) {
  const changes = useChangeCount(cents)

  return (
    <span
      className={cn(
        'shrink-0 rounded-xs bg-surface-raised px-sm py-2xs font-mono font-medium text-foreground',
        changes > 0 && 'animate-tick'
      )}
      // A new key restarts the light on every change.
      key={changes}
      {...(changes > 0 ? { 'data-ticked': '' } : {})}
    >
      {cents}¢
    </span>
  )
}

function When({ card }: { card: BoardCard }) {
  if (card.kind === 'updown') {
    return (
      <span className="font-mono">
        <MarketCountdown endsAt={Date.parse(card.closesAt)} /> left
      </span>
    )
  }
  if (card.kind === 'question') {
    if (card.closesAt === undefined) return null
    // The viewer's own clock, as with a game's start time.
    return (
      <time dateTime={card.closesAt} suppressHydrationWarning>
        Closes {closesOn.format(new Date(card.closesAt))}
      </time>
    )
  }
  // The list gives a start time and no score, so "Started" is all that is known.
  if (card.started) return <span className="text-positive">Started</span>
  // The viewer's own clock: the server and the browser may sit in different time zones.
  return (
    <time dateTime={card.startsAt} suppressHydrationWarning>
      {startsAt.format(new Date(card.startsAt))}
    </time>
  )
}

function MarketCard({ card }: { card: BoardCard }) {
  const tradeSlug = tradeSlugOf(card)
  return (
    <article
      aria-label={card.title}
      className={cn(
        'flex h-full flex-col gap-md rounded-md border bg-background/70 p-lg transition-[border-color,box-shadow] duration-200 ease-out-soft',
        tradeSlug === undefined
          ? 'border-border-subtle hover:border-highlight'
          : 'border-positive shadow-glow-edge'
      )}
    >
      <header className="flex items-center justify-between gap-md text-caption text-muted-foreground">
        <span className="truncate">{groupOf(card)}</span>
        <When card={card} />
      </header>
      <h3
        className={cn(
          'font-display text-heading-sm font-bold text-foreground',
          card.kind === 'question' && 'line-clamp-3'
        )}
      >
        {card.title}
      </h3>
      <ul className="flex flex-1 flex-col gap-xs">
        {sidesOf(card).map(side => (
          <li className="flex items-center justify-between gap-md text-label" key={side.label}>
            <span className="truncate text-foreground">{side.label}</span>
            {side.cents === undefined ? null : <Price cents={side.cents} />}
          </li>
        ))}
      </ul>
      <footer className="text-caption text-muted-foreground">
        {tradeSlug === undefined ? (
          <>
            {card.kind === 'question' && card.more > 0 ? `+${card.more} more · ` : ''}Shown from
            Gemini
          </>
        ) : (
          <Link
            className="rounded-xs font-medium text-foreground underline underline-offset-2 transition-colors hover:text-muted-foreground focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus-ring"
            href={`/market/${tradeSlug}`}
          >
            Trade on Kyros<span className="sr-only">{`: ${card.title}`}</span>
          </Link>
        )}
      </footer>
    </article>
  )
}

/**
 * Every up/down series and every game on Gemini, at the price last read, with a filter per
 * kind of market. The ones Kyros trades link to their market; the rest are shown, not traded.
 */
export function GeminiBoardSection({ board }: { board: GeminiBoard }) {
  const [filter, setFilter] = useState(ALL)
  const [shown, setShown] = useState(PAGE)

  // Tradable cards lead, in their existing order; then the other up/down cards, the question
  // cards, and last the other game cards.
  const tradable = [...board.upDown, ...board.games].filter(card => card.tradeSlug !== undefined)
  const cards: readonly BoardCard[] = [
    ...tradable,
    ...board.upDown.filter(card => card.tradeSlug === undefined),
    ...board.questions,
    ...board.games.filter(card => card.tradeSlug === undefined),
  ]
  const filters = filtersOf(cards)
  // A filter whose markets have all gone falls back to everything.
  const active = filters.some(item => item.name === filter) ? filter : ALL
  const matching = active === ALL ? cards : cards.filter(card => groupOf(card) === active)
  const hidden = Math.max(0, matching.length - shown)

  return (
    <section aria-labelledby="gemini-board-title" className="mt-4xl flex flex-col gap-xl">
      <header className="flex flex-col gap-sm">
        <h2
          className="font-display text-display-sm font-bold tracking-heading text-foreground"
          id="gemini-board-title"
        >
          Everything on Gemini
        </h2>
        <p className="max-w-prose text-body text-muted-foreground">
          Every market Gemini lists, at the price last read: up or down rounds, games, and questions
          on politics, weather, the economy and more. The ones marked Trade on Kyros run here today;
          the rest are shown, not traded.
        </p>
      </header>

      <fieldset className="flex flex-wrap gap-sm">
        <legend className="sr-only">Kind of market</legend>
        {filters.map(item => (
          <Button
            aria-pressed={item.name === active}
            key={item.name}
            onClick={() => {
              setFilter(item.name)
              setShown(PAGE)
            }}
            size="small"
            variant={item.name === active ? 'primary' : 'secondary'}
          >
            {item.name} <span className="font-mono">{item.count}</span>
          </Button>
        ))}
      </fieldset>

      <ul className="grid gap-md [grid-template-columns:repeat(auto-fill,minmax(15rem,1fr))]">
        {matching.slice(0, shown).map((card, index) => (
          <li
            className="animate-rise"
            key={card.id}
            style={{ animationDelay: `${Math.min(index, STAGGERED) * STAGGER_MS}ms` }}
          >
            <MarketCard card={card} />
          </li>
        ))}
      </ul>

      {hidden === 0 ? null : (
        <div>
          <Button onClick={() => setShown(shown + PAGE)} size="small" variant="secondary">
            Show {Math.min(PAGE, hidden)} more
          </Button>
        </div>
      )}

      <p className="text-caption text-muted-foreground">
        Read from Gemini&apos;s public list at {readAt.format(new Date(board.asOf))} UTC.
      </p>
    </section>
  )
}
