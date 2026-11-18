// @vitest-environment node
import { http, HttpResponse } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest'

vi.hoisted(() => {
  vi.stubEnv('EXCHANGE_API_URL', 'https://exchange.test')
  vi.stubEnv('HIDDEN_UNDERLYINGS', ' NBA ,')
})

import { seriesIdForIndex } from '@/features/market-detail/exchange-catalog.mapper'
import {
  loadActiveSeriesEvent,
  loadLiveSeriesEventBySlug,
  loadLiveSeriesEvents,
} from '@/server/silk-road.server'

const server = setupServer(
  http.get('https://exchange.test/api/series', () =>
    HttpResponse.json([
      {
        series: 'btc-2m',
        underlying: 'btc',
        duration_secs: 120,
        current_market_id: 3,
        opens_at_ms: 1_788_598_800_000,
        closes_at_ms: 1_788_598_920_000,
      },
      {
        series: 'nba-h2h',
        underlying: 'nba',
        duration_secs: 900,
        current_market_id: 7,
        opens_at_ms: 1_788_598_800_000,
        closes_at_ms: 1_788_599_700_000,
      },
    ])
  )
)

beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

describe('HIDDEN_UNDERLYINGS', () => {
  it('drops hidden series from the live catalog while keeping exchange indices', async () => {
    const live = await loadLiveSeriesEvents()
    expect(live.map(item => item.series.slug)).toEqual(['btc-2m'])
    expect(live[0]?.series.id).toBe(seriesIdForIndex(0))
  })

  it('refuses a hidden series by slug and by id', async () => {
    expect(await loadLiveSeriesEventBySlug('nba-h2h')).toBeUndefined()
    expect(await loadActiveSeriesEvent(seriesIdForIndex(1))).toBeUndefined()
    expect(await loadActiveSeriesEvent(seriesIdForIndex(0))).toBeDefined()
  })
})
