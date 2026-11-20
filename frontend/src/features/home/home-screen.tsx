import type { GeminiBoard } from './gemini-board-loader'
import { GeminiBoardSection } from './gemini-board-section'
import { HomeRefresh } from './home-refresh'
import { HomeHero, type HomeRecords, HowItWorks, JustSealed, TopStrategies } from './home-pulse'
import { HorizonScene } from './horizon-scene'
import type { ReplayIndexEntry } from '@/features/replay/replay'
import { ReplaysSection } from '@/features/replay/replays-section'
import { LiveHomeMarketCard } from './live-home-market-card'
import type { HomeMarketCardModel } from './types'
import { type VenueBoard, withoutListed } from './venue-board'
import { VenueBoardSection } from './venue-board-section'

export interface HomeScreenProps {
  /** Everything Gemini lists; absent when its public list could not be read. */
  board?: GeminiBoard | undefined
  cards: readonly HomeMarketCardModel[]
  /** Block explorer origin the sealed rounds link to. */
  explorerUrl?: string | undefined
  /** What is sealed on Arc; absent when the record could not be read in time. */
  records?: HomeRecords | undefined
  /** Finished games to play back, closest first; none on a build without them. */
  replays?: readonly ReplayIndexEntry[] | undefined
  /** Games other venues price in leagues Gemini does not list; absent when unread. */
  venues?: VenueBoard | undefined
}

export function HomeScreen({
  board,
  cards,
  explorerUrl,
  records,
  replays = [],
  venues,
}: HomeScreenProps) {
  const ranked = records !== undefined && records.rows.length > 0
  const sealed = records !== undefined && records.recent.length > 0 && explorerUrl !== undefined
  // Every league and category Gemini's own board carries, so none of them is shown as missing.
  const onGemini =
    board === undefined
      ? []
      : [...board.games.map(game => game.league), ...board.questions.map(card => card.category)]
  // "Not on Gemini" is only said when Gemini's own games were read to check it against.
  const checked = venues !== undefined && board !== undefined && board.games.length > 0
  const elsewhere = checked ? withoutListed(venues, onGemini) : undefined

  return (
    <main className="mx-auto w-full max-w-[90rem] px-4xl py-4xl">
      <HorizonScene />
      <HomeRefresh />
      <HomeHero totals={ranked ? records.totals : undefined} />

      <section aria-labelledby="markets" className="scroll-mt-below-nav">
        <h2
          className="mb-xl font-display text-display-sm font-bold tracking-heading text-foreground"
          id="markets"
        >
          Markets
        </h2>
        {cards.length === 0 ? (
          <p className="text-body text-muted-foreground">No live markets right now.</p>
        ) : (
          <div className="flex flex-wrap gap-xl">
            {cards.map(card => (
              <LiveHomeMarketCard card={card} key={card.slug} />
            ))}
          </div>
        )}
      </section>

      {replays.length === 0 ? null : (
        <div className="mt-4xl">
          <ReplaysSection games={replays} />
        </div>
      )}

      <div className="mt-4xl">
        <HowItWorks />
      </div>

      {ranked || sealed ? (
        <div className="mt-4xl grid items-start gap-4xl lg:grid-cols-2">
          {ranked ? <TopStrategies rows={records.rows} /> : null}
          {sealed ? <JustSealed explorerUrl={explorerUrl} recent={records.recent} /> : null}
        </div>
      ) : null}

      {board === undefined ||
      board.upDown.length + board.games.length + board.questions.length === 0 ? null : (
        <GeminiBoardSection board={board} />
      )}

      {elsewhere === undefined ? null : <VenueBoardSection board={elsewhere} />}
    </main>
  )
}
