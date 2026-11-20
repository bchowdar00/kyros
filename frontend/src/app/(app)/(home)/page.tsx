import type { Metadata } from 'next'

import { env } from '@/env'
import { HomeScreen } from '@/features/home/home-screen'
import { withKyrosGame } from '@/features/home/gemini-board'
import { ReplayIndexSchema } from '@/features/replay/replay'
import replayIndex from '@/features/replay/replay-index.json'
import { withinMs } from '@/lib/within-ms'
import { loadGeminiBoard } from '@/server/gemini-board.server'
import { loadHomeMarkets } from '@/server/home.server'
import { loadRecordBoard } from '@/server/record-board.server'
import { loadKeptVenueBoard } from '@/server/venue-board.server'

const description = 'Live prediction markets with automated trading strategies.'
/**
 * How long the page waits for the record on Arc. Only the first visitor after a start can wait
 * this long; everyone after is answered from the last read (`createRecordBoardLoader`).
 */
const RECORD_WAIT_MS = 1500
/** The same for Gemini's whole list, which is tens of megabytes when read afresh. */
const BOARD_WAIT_MS = 2500
/** And for the games other venues price, which the page is complete without. */
const VENUES_WAIT_MS = 1500

/** The finished games the page plays back; a build whose index does not read has none. */
const parsedReplays = ReplayIndexSchema.safeParse(replayIndex)
const replays = parsedReplays.success ? parsedReplays.data : []

export const metadata: Metadata = {
  alternates: { canonical: '/' },
  description,
  openGraph: { description, title: 'Kyros', type: 'website' },
  title: 'Markets',
  twitter: { card: 'summary_large_image' },
}

export default async function Home() {
  const [cards, board, record, venues] = await Promise.all([
    loadHomeMarkets(),
    withinMs(loadGeminiBoard(), BOARD_WAIT_MS),
    withinMs(loadRecordBoard(), RECORD_WAIT_MS),
    withinMs(loadKeptVenueBoard(), VENUES_WAIT_MS),
  ])
  // The one game Kyros trades is the card with team names where the others have Up and Down.
  const game = cards.find(card => card.priceChart === undefined)

  return (
    <HomeScreen
      board={
        board === undefined
          ? undefined
          : {
              ...board,
              games: withKyrosGame(
                board.games,
                game === undefined
                  ? undefined
                  : { slug: game.slug, teams: game.outcomes.map(outcome => outcome.label) }
              ),
            }
      }
      cards={cards}
      explorerUrl={env.NEXT_PUBLIC_EXPLORER_URL}
      records={record?.status === 'ok' ? record : undefined}
      replays={replays}
      venues={venues}
    />
  )
}
