import 'server-only'

import {
  type KalshiReading,
  kalshiHomeMarket,
  toKalshiHistory,
} from '@/features/market-detail/kalshi-history'
import { createKeptLoader } from '@/lib/kept-loader'

/** Kalshi's public REST host; the only host this loader calls. Read-only, no account. */
const API_URL = 'https://api.elections.kalshi.com/trade-api/v2'
const NBA_GAMES = 'KXNBAGAME'
const REQUEST_TIMEOUT_MS = 8000
const SECOND_MS = 1000
/** How far back the chart of a game opens: the half day before, minute by minute. */
const BACK_MS = 12 * 60 * 60_000
const FRESH_FOR_MS = 60_000
const FAILED_FOR_MS = 15_000
/** Past this the kept minutes end too long ago to be the start of a live line. */
const STALE_FOR_MS = 10 * 60_000

interface Teams {
  away: string
  home: string
}

async function requestJson(url: URL): Promise<unknown> {
  const response = await fetch(url, {
    cache: 'no-store',
    headers: { Accept: 'application/json' },
    signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
  })
  if (!response.ok) throw new Error(`${response.status} from ${url.host}`)
  return response.json()
}

async function readHistory(teams: Teams): Promise<KalshiReading[]> {
  const events = new URL(`${API_URL}/events`)
  events.searchParams.set('series_ticker', NBA_GAMES)
  events.searchParams.set('status', 'open')
  events.searchParams.set('with_nested_markets', 'true')
  events.searchParams.set('limit', '100')
  const market = kalshiHomeMarket(await requestJson(events), teams)
  if (market === undefined) throw new Error('Kalshi does not list this game')

  const end = Math.floor(Date.now() / SECOND_MS)
  const candles = new URL(`${API_URL}/series/${NBA_GAMES}/markets/${market}/candlesticks`)
  candles.searchParams.set('start_ts', String(end - BACK_MS / SECOND_MS))
  candles.searchParams.set('end_ts', String(end))
  candles.searchParams.set('period_interval', '1')
  const history = toKalshiHistory(await requestJson(candles))
  if (history.length === 0) throw new Error('Kalshi has no price history for this game')
  return history
}

const loaders = new Map<string, () => Promise<KalshiReading[] | undefined>>()

/**
 * Kalshi's price for the home side of a game over the last half day, for the start of its
 * line on the chart. One kept answer per game and server process, so page views never
 * multiply requests to Kalshi, and a read that fails never replaces a good one.
 */
export function loadKalshiGameHistory(teams: Teams): Promise<KalshiReading[] | undefined> {
  const key = `${teams.away}-${teams.home}`
  let loader = loaders.get(key)
  if (loader === undefined) {
    loader = createKeptLoader(() => readHistory(teams), {
      failedForMs: FAILED_FOR_MS,
      freshForMs: FRESH_FOR_MS,
      staleForMs: STALE_FOR_MS,
    })
    loaders.set(key, loader)
  }
  return loader()
}
