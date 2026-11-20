import 'server-only'

import { cache } from 'react'

import { withCardPast } from '@/features/home/card-past'
import type { HomeMarketCardModel } from '@/features/home/types'
import { toHomeMarketCard } from '@/features/market-detail/event-detail.mapper'
import { fixtureHomeCards } from '@/features/market-detail/fixture-cards'
import { withinMs } from '@/lib/within-ms'
import { fixturesEnabled } from '@/server/fixtures.server'
import { loadKalshiGameHistory } from '@/server/kalshi-history.server'
import { loadLiveSeriesEvents } from '@/server/silk-road.server'

/** How long the page waits for a game's past prices; the card is complete without them. */
const GAME_PAST_WAIT_MS = 1500

/** A game card opened with the price the game traded at before the page was asked for. */
async function withPastOf(card: HomeMarketCardModel): Promise<HomeMarketCardModel> {
  if (card.series === undefined) return card
  const home = card.series.find(line => line.side === 'home')
  const away = card.series.find(line => line.side === 'away')
  if (home === undefined || away === undefined) return card
  const past = await withinMs(
    loadKalshiGameHistory({ away: away.label, home: home.label }).catch(() => undefined),
    GAME_PAST_WAIT_MS
  )
  return withCardPast(card, past)
}

export const loadHomeMarkets = cache(async (): Promise<readonly HomeMarketCardModel[]> => {
  if (fixturesEnabled()) {
    return fixtureHomeCards()
  }

  const live = await loadLiveSeriesEvents()
  const cards: HomeMarketCardModel[] = []

  for (const item of live) {
    const card = toHomeMarketCard(item.series, item.detail)

    if (card !== undefined) {
      cards.push(card)
    }
  }

  return Promise.all(cards.map(withPastOf))
})
