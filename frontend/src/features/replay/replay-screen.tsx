'use client'

import { type LineSeries, MarketChart } from '@kyros/ui/charts/market-chart'
import { AnimatedNumber } from '@kyros/ui/components/animated-number'
import { cn } from '@kyros/ui/lib/cn'
import { useBrowserTimeZone } from '@kyros/ui/lib/use-browser-time-zone'
import Link from 'next/link'

import { priceLine, type ReplayIndexEntry, replayDay } from './replay'
import { useReplay } from './use-replay'

const panel = 'rounded-md border border-border-subtle bg-background/70'
const sources =
  'Finished NBA games played back play by play, with the breaks shortened. Scores from ESPN, prices from Kalshi.'

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

/** The full-page replay of one finished game: its score, Kalshi's price, and the scoring plays. */
export function ReplayScreen({ entry, index }: { entry: ReplayIndexEntry; index: number }) {
  const timeZone = useBrowserTimeZone()
  const { failed, game, position, state, nowMs } = useReplay(entry, index)

  const points = game === undefined ? [] : priceLine(game, position, nowMs)
  const series: LineSeries[] = [
    { color: 'var(--market-home)', id: 'home', label: `${entry.home.code} win`, points },
  ]
  const range = { end: Math.max(nowMs, position.startedAtMs + 1), start: position.startedAtMs }

  return (
    <main className="mx-auto flex max-w-screen-md flex-col gap-2xl px-lg py-2xl">
      <header className="flex flex-col gap-sm">
        <span className="text-label text-muted-foreground">Replay · {replayDay(entry.date)}</span>
        <h1 className="font-display text-display-md font-bold tracking-display text-foreground">
          {entry.away.name} at {entry.home.name}
        </h1>
        <p className="text-caption text-muted-foreground">
          It already happened: nothing here can be traded.
        </p>
        <div className="flex items-baseline gap-2xl font-mono">
          <span className="flex items-baseline gap-md">
            <span className="text-body text-muted-foreground">{entry.away.code}</span>
            <span className="text-display-md font-bold text-foreground">
              {state === undefined ? '–' : <Score value={state.away} />}
            </span>
          </span>
          <span className="flex items-baseline gap-md">
            <span className="text-body text-muted-foreground">{entry.home.code}</span>
            <span className="text-display-md font-bold text-foreground">
              {state === undefined ? '–' : <Score value={state.home} />}
            </span>
          </span>
        </div>
        <span className="text-label text-muted-foreground">
          {/* Until the game has loaded there is no score to show, least of all the final one. */}
          {state === undefined
            ? failed
              ? 'Replay unavailable'
              : 'Loading replay'
            : position.ended
              ? 'Final'
              : `Q${state.period} ${state.clock}`}
        </span>
      </header>

      <section aria-labelledby="price-title" className="flex flex-col gap-md">
        <h2 id="price-title" className="text-heading-sm font-bold text-foreground">
          {entry.home.code} win price
        </h2>
        <div className={cn(panel, 'h-chart-lg w-full p-md')}>
          <MarketChart
            kind="probability"
            range={range}
            series={series}
            timeZone={timeZone}
            title={`${entry.home.code} win price over the game`}
            valueDomain={[0, 100]}
          />
        </div>
      </section>

      <section aria-labelledby="scoring-title" className="flex flex-col gap-md">
        <h2 id="scoring-title" className="text-heading-sm font-bold text-foreground">
          Scoring plays
        </h2>
        {state === undefined || state.scoring.length === 0 ? (
          <p className="text-caption text-muted-foreground">No scoring plays yet.</p>
        ) : (
          <ul
            aria-label="Scoring plays"
            className={cn(panel, 'flex flex-col divide-y divide-border-subtle')}
          >
            {state.scoring.map(play => {
              const code = play.team === 'home' ? entry.home.code : entry.away.code
              return (
                <li
                  className="flex animate-rise px-lg py-md font-mono text-body text-foreground"
                  // A play keeps its row as newer ones arrive above it.
                  key={`${play.atMs}:${play.team}:${play.home}:${play.away}`}
                >
                  {`${code} +${play.points} · Q${play.period} ${play.clock} · ${play.away}–${play.home}`}
                </li>
              )
            })}
          </ul>
        )}
      </section>

      <footer className="flex flex-col gap-sm text-caption text-muted-foreground">
        <p>{sources}</p>
        <Link
          className="rounded-xs text-foreground underline underline-offset-2 transition-colors hover:text-muted-foreground focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus-ring"
          href="/"
        >
          Back to markets
        </Link>
      </footer>
    </main>
  )
}
