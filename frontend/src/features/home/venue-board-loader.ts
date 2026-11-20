import { type VenueBoard, type VenueGame, VENUE_LEAGUES, toVenueGames } from './venue-board'

/**
 * A loader for the games Gemini does not list, read from Kalshi's public `/events` list, one
 * request per league in parallel. Read-only and public; nothing here touches an account. A
 * league whose read fails or yields no games is left out, so one failing never empties the
 * board; `undefined` only when no league is left.
 */

/** Kalshi's public REST host; the only host this loader calls. */
const EVENTS_URL = 'https://api.elections.kalshi.com/trade-api/v2/events'
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

  return leagues.length === 0 ? undefined : { leagues }
}
