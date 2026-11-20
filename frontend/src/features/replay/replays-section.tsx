import type { ReplayIndexEntry } from './replay'
import { ReplayCard } from './replay-card'

const sectionTitle = 'font-display text-heading-md font-bold tracking-heading text-foreground'

/** How many games the home page shows: two rows at its widest. */
const SHOWN = 8

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
          NBA games
        </h2>
        <p className="max-w-prose text-caption text-muted-foreground">
          Play by play, with the score and the price.
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
