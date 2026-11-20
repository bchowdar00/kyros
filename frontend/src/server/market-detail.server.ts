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
import { toStrategyCardNames } from '@/features/market-detail/strategy-card-names'
import { toPublicStrategies } from '@/features/market-detail/strategy-template.mapper'
import type { MarketDetail } from '@/features/market-detail/types'
import { withVenuePast } from '@/features/market-detail/venue-lines'
import { withinMs } from '@/lib/within-ms'
import { fixturesEnabled } from '@/server/fixtures.server'
import { loadKalshiGameHistory } from '@/server/kalshi-history.server'
import { loadLiveSeriesEventBySlug } from '@/server/silk-road.server'

const RECENT_ROUNDS = 15
/** How long a game's page waits for Kalshi's past prices; it is complete without them. */
const VENUE_PAST_WAIT_MS = 1500

/** A game's chart opened with Kalshi's prices from before the page was asked for. */
async function withVenuePastOf(market: MarketDetail): Promise<MarketDetail> {
  const { probabilityChart, scoreboard } = market
  if (probabilityChart === undefined || scoreboard === undefined) return market
  const past = await withinMs(
    loadKalshiGameHistory(scoreboard).catch(() => undefined),
    VENUE_PAST_WAIT_MS
  )
  return {
    ...market,
    probabilityChart: withVenuePast(probabilityChart, market.slug, 'kalshi', past),
  }
}

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
    ...(templates.ok
      ? {
          chatPanel: toChatPanel(toPublicStrategies(templates.data)),
          // Every card, deployable now or not: a strategy already running keeps its card's name.
          strategyCardNames: toStrategyCardNames(templates.data),
        }
      : {}),
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
  if (market === undefined) return undefined
  const [extras, opened] = await Promise.all([
    withExchangeExtras(live.gemini === undefined ? market : { ...market, gemini: live.gemini }),
    withVenuePastOf(market),
  ])
  return opened.probabilityChart === undefined
    ? extras
    : { ...extras, probabilityChart: opened.probabilityChart }
})
