// @vitest-environment node
import { http, HttpResponse } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest'

vi.hoisted(() => {
  vi.stubEnv('EXCHANGE_API_URL', 'https://exchange.test')
  vi.stubEnv('HIDDEN_UNDERLYINGS', ' ETH ,')
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
      // The hidden series comes first, so the visible one sits at exchange index 1. Its slug
      // does not begin with its underlying, so hiding by slug or slug prefix cannot pass.
      {
        series: 'second-5m',
        underlying: 'eth',
        duration_secs: 300,
        current_market_id: 3,
        opens_at_ms: 1_788_598_800_000,
        closes_at_ms: 1_788_599_100_000,
      },
      {
        series: 'btc-15m',
        underlying: 'btc',
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
    expect(live.map(item => item.series.slug)).toEqual(['btc-15m'])
    expect(live[0]?.series.id).toBe(seriesIdForIndex(1))
  })

  it('refuses a hidden series by slug and by id', async () => {
    expect(await loadLiveSeriesEventBySlug('second-5m')).toBeUndefined()
    expect(await loadActiveSeriesEvent(seriesIdForIndex(0))).toBeUndefined()
    expect(await loadActiveSeriesEvent(seriesIdForIndex(1))).toBeDefined()
    expect(await loadLiveSeriesEventBySlug('btc-15m')).toBeDefined()
  })
})
