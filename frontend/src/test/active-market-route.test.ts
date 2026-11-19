// @vitest-environment node
import { http, HttpResponse } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest'

vi.hoisted(() => vi.stubEnv('EXCHANGE_API_URL', 'https://exchange.test'))

import { GET } from '@/app/api/series/[seriesId]/active-market/route'
import { eventIdForMarket } from '@/features/market-detail/exchange-catalog.mapper'

const server = setupServer()
const request = new Request('http://localhost/api/series/100002/active-market')
const context = { params: Promise.resolve({ seriesId: '100002' }) }

function mockSeries(currentMarketId: number | null): void {
  server.use(
    http.get('https://exchange.test/api/series', () =>
      HttpResponse.json([
        {
          series: 'btc-5m',
          duration_secs: 300,
          current_market_id: 3,
          opens_at_ms: 1_788_598_800_000,
          closes_at_ms: 1_788_598_920_000,
        },
        {
          series: 'btc-15m',
          duration_secs: 900,
          current_market_id: currentMarketId,
          opens_at_ms: currentMarketId === null ? null : 1_788_598_800_000,
          closes_at_ms: currentMarketId === null ? null : 1_788_599_700_000,
        },
      ])
    )
  )
}

beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => {
  server.close()
  vi.unstubAllEnvs()
})

describe('active market endpoint', () => {
  it('returns the requested series round as an uncached identity', async () => {
    mockSeries(9)

    const response = await GET(request, context)
    expect(response.status).toBe(200)
    expect(response.headers.get('cache-control')).toBe('no-store')
    expect(await response.json()).toEqual({ eventId: eventIdForMarket(9) })
  })

  it('does not switch to a series before its first round opens', async () => {
    mockSeries(null)
    expect((await GET(request, context)).status).toBe(503)
  })

  it('rejects invalid series IDs before contacting the backend', async () => {
    expect((await GET(request, { params: Promise.resolve({ seriesId: 'invalid' }) })).status).toBe(
      400
    )
  })

  it('reports backend failure without inventing an active market', async () => {
    server.use(
      http.get('https://exchange.test/api/series', () => new HttpResponse(null, { status: 503 }))
    )
    expect((await GET(request, context)).status).toBe(503)
  })
})
