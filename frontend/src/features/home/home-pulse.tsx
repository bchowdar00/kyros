import { cents, createMoneyFormatter } from '@kyros/money'
import { AnimatedNumber } from '@kyros/ui/components/animated-number'
import { Button } from '@kyros/ui/components/button'
import { cn } from '@kyros/ui/lib/cn'
import Link from 'next/link'

import type { NamedRecentSeal, NamedRecordBoardRow } from '@/features/records/record-board-loader'
import { atUtc, seriesLabel, strategyName, under, winRate } from '@/features/records/record-format'

/** The record as the home page uses it: the leaderboard's own rows and newest rounds. */
export interface HomeRecords {
  recent: readonly NamedRecentSeal[]
  rows: readonly NamedRecordBoardRow[]
  totals: { rounds: number; strategies: number; wonRounds: number }
}

const money = createMoneyFormatter('en-US')
/** Strategies shown before the link to the whole leaderboard. */
const TOP = 3
/** Sealed rounds shown in the feed. */
const FEED = 5
const STAGGER_MS = 40

const focusRing =
  'focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus-ring'
const quietLink = `rounded-xs text-foreground underline underline-offset-2 transition-colors hover:text-muted-foreground ${focusRing}`
const panel = 'rounded-md border border-border-subtle bg-background/70'
const sectionTitle = 'font-display text-heading-md font-bold tracking-heading text-foreground'

function result(pnlCents: number): string {
  return money.usd(cents(pnlCents), { signDisplay: 'exceptZero' })
}

function resultTone(pnlCents: number): string | false {
  return (pnlCents > 0 && 'text-positive') || (pnlCents < 0 && 'text-negative')
}

/** A count that rolls to its new value when the record changes; read aloud as a plain number. */
function Count({ value }: { value: number }) {
  return (
    <>
      <span className="sr-only">{money.count(value)}</span>
      <span aria-hidden="true">
        <AnimatedNumber locales="en-US" value={value} />
      </span>
    </>
  )
}

/** What Kyros is, where to start, and how much is on the record so far. */
export function HomeHero({ totals }: { totals?: HomeRecords['totals'] | undefined }) {
  const stats =
    totals === undefined
      ? []
      : [
          { label: 'Strategies on the record', value: totals.strategies },
          { label: 'Rounds sealed', value: totals.rounds },
          { label: 'Rounds won', value: totals.wonRounds },
        ]

  return (
    <section className="flex flex-col items-center gap-xl pt-4xl pb-4xl text-center">
      <h1 className="max-w-[18ch] font-display text-display-lg font-bold tracking-display text-foreground">
        Put a strategy to work on <span className="text-positive">live markets</span>
      </h1>
      <p className="max-w-prose text-prose text-muted-foreground">
        Describe a strategy in plain words and let it trade every round for you.
      </p>
      <div className="flex flex-wrap items-center justify-center gap-md">
        <Button asChild size="large">
          <a href="#markets">Pick a market</a>
        </Button>
        <Button asChild size="large" variant="secondary">
          <Link href="/leaderboard">See the leaderboard</Link>
        </Button>
      </div>
      <p className="text-caption text-muted-foreground">Pretend money, live Gemini prices.</p>
      {stats.length === 0 ? null : (
        <section aria-label="Sealed on Arc so far" className="mt-xl">
          <dl className="flex flex-wrap justify-center divide-x divide-border-subtle">
            {stats.map(stat => (
              <div className="flex flex-col-reverse gap-xs px-4xl" key={stat.label}>
                <dt className="text-label text-muted-foreground">{stat.label}</dt>
                <dd className="font-display text-display-md font-bold text-foreground">
                  <Count value={stat.value} />
                </dd>
              </div>
            ))}
          </dl>
        </section>
      )}
    </section>
  )
}

const STEPS = [
  {
    body: 'Write the strategy as a sentence on any market, or copy one from the leaderboard.',
    title: 'Say what you want',
  },
  {
    body: 'An LLM writes it. A compiler checks it before any money moves.',
    title: 'Checked before it trades',
  },
  {
    body: 'It runs in its own account, holding only the budget you give it. Every round it trades is sealed on Arc.',
    title: 'Sealed where anyone can look',
  },
] as const

/** The three things that happen between a sentence and a sealed round. */
export function HowItWorks() {
  return (
    <section aria-labelledby="how-it-works-title" className="flex flex-col gap-xl">
      <h2 className={sectionTitle} id="how-it-works-title">
        How it works
      </h2>
      <ol className="grid gap-md md:grid-cols-3">
        {STEPS.map((step, index) => (
          <li
            className={cn(
              panel,
              'flex animate-rise flex-col gap-md p-xl shadow-glow-inner-up transition-[border-color] duration-200 ease-out-soft hover:border-highlight'
            )}
            key={step.title}
            style={{ animationDelay: `${index * STAGGER_MS}ms` }}
          >
            <span aria-hidden="true" className="font-mono text-label text-positive">
              {`0${index + 1}`}
            </span>
            <h3 className="font-display text-heading-md font-bold text-foreground">{step.title}</h3>
            <p className="text-body text-muted-foreground">{step.body}</p>
          </li>
        ))}
      </ol>
    </section>
  )
}

/** The best three strategies by sealed result, each open to copying when it runs a public card. */
export function TopStrategies({ rows }: { rows: readonly NamedRecordBoardRow[] }) {
  return (
    <section aria-labelledby="top-strategies-title" className="flex flex-col gap-lg">
      <header className="flex items-baseline justify-between gap-md">
        <h2 className={sectionTitle} id="top-strategies-title">
          Top strategies
        </h2>
        <Link className={cn('text-label', quietLink)} href="/leaderboard">
          Full leaderboard
        </Link>
      </header>
      <ol className="flex flex-col gap-sm">
        {rows.slice(0, TOP).map((row, index) => {
          const name = strategyName(row)
          return (
            <li
              className={cn(
                panel,
                'flex animate-rise items-center gap-lg p-lg transition-[border-color] duration-200 ease-out-soft hover:border-highlight'
              )}
              key={row.deploymentId}
              style={{ animationDelay: `${index * STAGGER_MS}ms` }}
            >
              <span aria-hidden="true" className="font-mono text-heading-md text-positive">
                {index + 1}
              </span>
              <div className="flex min-w-0 flex-1 flex-col gap-2xs">
                <h3 className="truncate font-display text-heading-sm font-bold text-foreground">
                  {name}
                </h3>
                <p className="text-caption text-muted-foreground">
                  {row.series === undefined ? '' : `${seriesLabel(row.series)} · `}
                  {winRate(row)} of {money.count(row.rounds)} rounds won
                </p>
              </div>
              <span className={cn('font-mono text-body font-medium', resultTone(row.pnlCents))}>
                {result(row.pnlCents)}
              </span>
              {row.copy === undefined || row.series === undefined ? null : (
                <Button asChild size="small">
                  <Link
                    href={{ pathname: `/market/${row.series}`, query: { copy: row.copy.title } }}
                  >
                    Copy<span className="sr-only">: {name}</span>
                  </Link>
                </Button>
              )}
            </li>
          )
        })}
      </ol>
    </section>
  )
}

/** The newest rounds any strategy sealed on Arc, each with its result and its transaction. */
export function JustSealed({
  explorerUrl,
  recent,
}: {
  explorerUrl: string
  recent: readonly NamedRecentSeal[]
}) {
  return (
    <section aria-labelledby="just-sealed-title" className="flex flex-col gap-lg">
      <header className="flex items-center gap-md">
        <h2 className={sectionTitle} id="just-sealed-title">
          Just sealed
        </h2>
        {/* The newest value's dot, as on the chart: one ring that fades out, again and again. */}
        <span aria-hidden="true" className="relative flex size-[0.5rem]">
          <span className="absolute inset-0 rounded-full bg-positive motion-safe:animate-chart-pulse" />
          <span className="relative size-full rounded-full bg-positive" />
        </span>
      </header>
      <ul className={cn(panel, 'flex flex-col divide-y divide-border-subtle')}>
        {recent.slice(0, FEED).map((seal, index) => {
          const name = strategyName(seal)
          const where = seal.series === undefined ? '' : `${seriesLabel(seal.series)} · `
          return (
            <li
              className="flex animate-rise items-center gap-lg px-lg py-md"
              key={`${seal.deploymentId}:${seal.roundId}`}
              style={{ animationDelay: `${index * STAGGER_MS}ms` }}
            >
              <div className="flex min-w-0 flex-1 flex-col gap-2xs">
                <span className="truncate text-label font-semibold text-foreground">{name}</span>
                <span className="text-caption text-muted-foreground">
                  {where}round {seal.roundId}
                </span>
              </div>
              <span className={cn('font-mono text-body font-medium', resultTone(seal.pnlCents))}>
                {result(seal.pnlCents)}
              </span>
              <div className="flex flex-col items-end gap-2xs text-caption text-muted-foreground">
                <span className="font-mono">{atUtc(seal.at)}</span>
                <a
                  className={quietLink}
                  href={under(explorerUrl, `tx/${seal.transactionHash}`)}
                  rel="noopener noreferrer"
                  target="_blank"
                >
                  View on Arc
                  <span className="sr-only">
                    : round {seal.roundId} of {name}
                  </span>
                </a>
              </div>
            </li>
          )
        })}
      </ul>
    </section>
  )
}
