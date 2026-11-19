// @vitest-environment node
import { http, HttpResponse } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest'

vi.hoisted(() => {
  vi.stubEnv('EXCHANGE_API_URL', 'https://exchange.test')
})

import { loadGameSeriesBySlug, loadLiveSeriesEvents } from '@/server/silk-road.server'

const noRound = { current_market_id: null, opens_at_ms: null, closes_at_ms: null }
const openRound = {
  current_market_id: 7,
  opens_at_ms: 1_788_598_800_000,
  closes_at_ms: 1_788_599_700_000,
}

const server = setupServer(
  http.get('https://exchange.test/api/series', () =>
    HttpResponse.json([
      { series: 'btc-5m', underlying: 'btc', duration_secs: 300, ...noRound },
      { series: 'btc-15m', underlying: 'btc', duration_secs: 900, ...openRound },
      { series: 'nba-h2h', underlying: 'nba', duration_secs: 300, ...noRound },
      {
        series: 'nba-second',
        underlying: 'nba',
        duration_secs: 300,
        ...openRound,
        // A number the exchange could not write (it serializes NaN as null).
        reference: { home_team: 'DEN', away_team: 'BOS', pregame_home_prob: null, game_seq: 1 },
      },
    ])
  )
)

beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

describe('series with no open round', () => {
  it('keeps the catalogue when one round reference cannot be read', async () => {
    const live = await loadLiveSeriesEvents()
    expect(live.map(item => item.series.slug)).toEqual(['btc-15m'])
  })

  it('resolves a game series that is waiting for its next game', async () => {
    const waiting = await loadGameSeriesBySlug('nba-h2h')
    expect(waiting?.series.slug).toBe('nba-h2h')
    expect(waiting?.series.title).toBe('NBA game winner')
  })

  it('does not resolve a Bitcoin series between rounds, or an unknown slug', async () => {
    expect(await loadGameSeriesBySlug('btc-5m')).toBeUndefined()
    expect(await loadGameSeriesBySlug('eth-5m')).toBeUndefined()
  })
})
