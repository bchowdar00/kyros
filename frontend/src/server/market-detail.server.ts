import 'server-only'

import { createRustExchangeClient } from '@kyros/api'
import { cache } from 'react'

import { env } from '@/env'
import { toChatPanel, toMarketDetail } from '@/features/market-detail/event-detail.mapper'
import {
  toRecentMarketResults,
  toStatsSignals,
} from '@/features/market-detail/exchange-extras.mapper'
import { getMarketDetailFixture } from '@/features/market-detail/market-detail.fixture'
import { toPublicStrategies } from '@/features/market-detail/strategy-template.mapper'
import type { MarketDetail } from '@/features/market-detail/types'
import { fixturesEnabled } from '@/server/fixtures.server'
import { loadLiveSeriesEventBySlug } from '@/server/silk-road.server'

const RECENT_ROUNDS = 15

function exchangeClient(baseUrl: string) {
  return createRustExchangeClient({
    baseUrl,
    fetch: (input, init) => fetch(input, { ...init, cache: 'no-store' }),
    retry: { attempts: 1 },
    timeoutMs: 2_000,
  })
}

/**
 * Round history, stats and the strategy cards come from the exchange. History and stats are
 * decoration, so a failed read leaves the chrome in place; a failed catalogue read leaves the
 * strategies column unavailable (`toChatPanel`) rather than empty.
 */
async function withExchangeExtras(market: MarketDetail): Promise<MarketDetail> {
  if (env.EXCHANGE_API_URL === undefined) {
    return market
  }

  const exchange = exchangeClient(env.EXCHANGE_API_URL)
  const [history, stats, templates] = await Promise.all([
    exchange.getMarketsHistory(market.slug, RECENT_ROUNDS),
    exchange.getMarketStats(market.slug),
    exchange.getStrategyTemplates(market.slug),
  ])

  return {
    ...market,
    ...(stats.ok ? { signals: toStatsSignals(stats.data) } : {}),
    ...(market.priceChart === undefined
      ? {}
      : {
          priceChart: {
            ...market.priceChart,
            ...(history.ok ? { recentMarketResults: toRecentMarketResults(history.data) } : {}),
          },
        }),
    ...(templates.ok ? { chatPanel: toChatPanel(toPublicStrategies(templates.data)) } : {}),
  }
}

export const loadMarketDetail = cache(async (slug: string): Promise<MarketDetail | undefined> => {
  if (fixturesEnabled()) {
    return getMarketDetailFixture(slug)
  }

  const live = await loadLiveSeriesEventBySlug(slug)

  if (live === undefined) {
    return undefined
  }

  const market = toMarketDetail(live.series, live.detail, undefined, live.gemini)

  // History comes with the `/ws/price` snapshot in the browser; nothing to preload here.
  // The Gemini block seeds the browser's poll of where the book comes from.
  return market === undefined
    ? undefined
    : withExchangeExtras(live.gemini === undefined ? market : { ...market, gemini: live.gemini })
})
