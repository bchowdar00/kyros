import { toKalshiSparks } from '@/features/market-detail/kalshi-history'

import {
  type VenueBoard,
  type VenueGame,
  VENUE_LEAGUES,
  toVenueGames,
  withSparks,
} from './venue-board'

/**
 * A loader for the games Gemini does not list, read from Kalshi's public `/events` list, one
 * request per league in parallel. Read-only and public; nothing here touches an account. A
 * league whose read fails or yields no games is left out, so one failing never empties the
 * board; `undefined` only when no league is left.
 */

/** Kalshi's public REST host; the only host this loader calls. */
const EVENTS_URL = 'https://api.elections.kalshi.com/trade-api/v2/events'
const HISTORY_URL = 'https://api.elections.kalshi.com/trade-api/v2/markets/candlesticks'
/** Each card's line: the last two days, hour by hour. */
const SPARK_BACK_SECONDS = 48 * 60 * 60
const SPARK_STEP_MINUTES = '60'
const SECOND_MS = 1000
const REQUEST_TIMEOUT_MS = 15_000
/** A full page of a league's open games; a league never has more than this. */
const PAGE_SIZE = '100'

async function requestJson(url: URL): Promise<unknown> {
  const response = await fetch(url, {
    cache: 'no-store',
    headers: { Accept: 'application/json' },
    signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
  })
  if (!response.ok) throw new Error(`${response.status} from ${url.host}`)
  return response.json()
}

async function loadLeague(series: string): Promise<VenueGame[]> {
  const url = new URL(EVENTS_URL)
  url.searchParams.set('series_ticker', series)
  url.searchParams.set('status', 'open')
  url.searchParams.set('with_nested_markets', 'true')
  url.searchParams.set('limit', PAGE_SIZE)
  return toVenueGames(await requestJson(url))
}

export async function loadVenueBoard(): Promise<VenueBoard | undefined> {
  const reads = await Promise.allSettled(VENUE_LEAGUES.map(league => loadLeague(league.series)))

  const leagues = VENUE_LEAGUES.flatMap((league, index) => {
    const read = reads[index]
    if (read === undefined || read.status !== 'fulfilled' || read.value.length === 0) return []
    return [{ games: read.value, label: league.label }]
  })

  if (leagues.length === 0) return undefined
  const board = { leagues }
  // The lines are decoration: a failed read leaves the board as it is, prices and all.
  return withSparks(board, await loadSparks(board).catch(() => ({})))
}

async function loadSparks(board: VenueBoard): Promise<ReturnType<typeof toKalshiSparks>> {
  const tickers = board.leagues.flatMap(league => league.games.flatMap(game => game.market ?? []))
  if (tickers.length === 0) return {}
  const end = Math.floor(Date.now() / SECOND_MS)
  const url = new URL(HISTORY_URL)
  url.searchParams.set('market_tickers', tickers.join(','))
  url.searchParams.set('start_ts', String(end - SPARK_BACK_SECONDS))
  url.searchParams.set('end_ts', String(end))
  url.searchParams.set('period_interval', SPARK_STEP_MINUTES)
  return toKalshiSparks(await requestJson(url))
}
