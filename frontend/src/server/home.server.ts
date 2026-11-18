import 'server-only'

import { cache } from 'react'

import type { HomeMarketCardModel } from '@/features/home/types'
import { toHomeMarketCard } from '@/features/market-detail/event-detail.mapper'
import { fixtureHomeCards } from '@/features/market-detail/fixture-cards'
import { fixturesEnabled } from '@/server/fixtures.server'
import { loadLiveSeriesEvents } from '@/server/silk-road.server'

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

  return cards
})
