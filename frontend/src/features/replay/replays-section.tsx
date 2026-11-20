import type { ReplayIndexEntry } from './replay'
import { ReplayCard } from './replay-card'

const sectionTitle = 'font-display text-heading-md font-bold tracking-heading text-foreground'

/** How many replays the home page shows. */
const SHOWN = 4

/**
 * The home page's band of replaying games: the closest finished games, played
 * back on a loop so the page always has a score and a price moving.
 */
export function ReplaysSection({ games }: { games: readonly ReplayIndexEntry[] }) {
  if (games.length === 0) return null

  return (
    <section aria-labelledby="replays-title" className="flex flex-col gap-lg">
      <header className="flex flex-col gap-xs">
        <h2 className={sectionTitle} id="replays-title">
          Game replays
        </h2>
        <p className="max-w-prose text-caption text-muted-foreground">
          Finished NBA games played back play by play, with the breaks shortened. Scores from ESPN,
          prices from Kalshi.
        </p>
      </header>
      <ul className="grid gap-md sm:grid-cols-2 lg:grid-cols-4">
        {games.slice(0, SHOWN).map((entry, index) => (
          <li key={entry.id}>
            <ReplayCard entry={entry} index={index} />
          </li>
        ))}
      </ul>
    </section>
  )
}
