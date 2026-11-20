import type { VenueBoard } from './venue-board'

/**
 * The games Gemini does not list, with Kalshi's prices, grouped by league. To watch only: no
 * links, no buttons, no trade. The cards reuse the Gemini board's look so the two read as one
 * board.
 */
export function VenueBoardSection({ board }: { board: VenueBoard }) {
  return (
    <section aria-labelledby="venue-board-title" className="mt-4xl flex flex-col gap-xl">
      <header className="flex flex-col gap-sm">
        <h2
          className="font-display text-display-sm font-bold tracking-heading text-foreground"
          id="venue-board-title"
        >
          More games, priced on Kalshi
        </h2>
        <p className="max-w-prose text-body text-muted-foreground">
          Gemini does not list these yet. The prices are Kalshi&apos;s, shown here to watch, not to
          trade.
        </p>
      </header>

      {board.leagues.map(league => (
        <div className="flex flex-col gap-lg" key={league.label}>
          <h3 className="font-display text-heading-md font-bold text-foreground">{league.label}</h3>
          <ul className="grid gap-md [grid-template-columns:repeat(auto-fill,minmax(15rem,1fr))]">
            {league.games.map(game => (
              <li key={game.id}>
                <article
                  aria-label={game.title}
                  className="flex h-full flex-col gap-md rounded-md border border-border-subtle bg-background/70 p-lg"
                >
                  <header className="flex items-center justify-between gap-md text-caption text-muted-foreground">
                    <span className="truncate">Kalshi</span>
                    {game.when === undefined ? null : <span>{game.when}</span>}
                  </header>
                  <h4 className="font-display text-heading-sm font-bold text-foreground">
                    {game.title}
                  </h4>
                  <ul className="flex flex-1 flex-col gap-xs">
                    {game.outcomes.map(outcome => (
                      <li
                        className="flex items-center justify-between gap-md text-label"
                        key={outcome.label}
                      >
                        <span className="truncate text-foreground">{outcome.label}</span>
                        <span className="shrink-0 rounded-xs bg-surface-raised px-sm py-2xs font-mono font-medium text-foreground">
                          {outcome.percent}%
                        </span>
                      </li>
                    ))}
                  </ul>
                </article>
              </li>
            ))}
          </ul>
        </div>
      ))}
    </section>
  )
}
