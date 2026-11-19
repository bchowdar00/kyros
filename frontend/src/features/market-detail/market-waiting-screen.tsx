import { MarketWaitingRefresh } from './market-waiting-refresh'

/** Shown while a listed series has no open round; it refreshes until the next game opens. */
const NO_GAME_SENTENCE = 'No game is open right now. The next one opens when Gemini lists it.'

/**
 * The market page for a series the exchange lists but which has no round right now. It
 * answers 200 (the series exists) and refreshes itself until a game opens.
 */
export function MarketWaitingScreen({ title }: { title: string }) {
  return (
    <section aria-label="Waiting for the next game" className="flex flex-col gap-lg px-4xl py-5xl">
      <h1 className="font-display text-display-md font-bold tracking-display text-foreground">
        {title}
      </h1>
      <p className="max-w-prose text-body font-regular text-muted-foreground">{NO_GAME_SENTENCE}</p>
      <MarketWaitingRefresh />
    </section>
  )
}
