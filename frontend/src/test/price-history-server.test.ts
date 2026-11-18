// @vitest-environment node
import { http, HttpResponse } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest'

vi.hoisted(() => vi.stubEnv('NEXT_PUBLIC_PRICE_FEEDS_URL', 'https://price-feeds.test'))
import { loadPriceHistory } from '@/server/price-history.server'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => {
  server.close()
  vi.unstubAllEnvs()
})

describe('price history', () => {
  it('loads the trailing window and drops grid points outside observed coverage', async () => {
    server.use(
      http.get('https://price-feeds.test/v1/prices', ({ request }) => {
        const query = new URL(request.url).searchParams
        expect(query.get('symbol')).toBe('Crypto.BTC/USD')
        expect(Number(query.get('to')) - Number(query.get('from'))).toBe(300)
        expect(query.get('resolution')).toBe('300')
        return HttpResponse.json({
          s: 'ok',
          firstTickAtUs: 2_000_000,
          lastTickAtUs: 3_500_000,
          points: [1, 2, 3, 4].map(t => ({ t, mantissa: '7965821', expo: -2 })),
        })
      })
    )
    expect(await loadPriceHistory('Crypto.BTC/USD', 300_000)).toEqual([
      { time: 2_000, value: 79_658.21 },
      { time: 3_000, value: 79_658.21 },
    ])
  })

  it('leaves missing history empty when the request fails or the payload is invalid', async () => {
    server.use(
      http.get('https://price-feeds.test/v1/prices', () => new HttpResponse(null, { status: 503 }))
    )
    expect(await loadPriceHistory('Crypto.BTC/USD', 120_000)).toEqual([])
    server.use(
      http.get('https://price-feeds.test/v1/prices', () =>
        HttpResponse.json({ s: 'ok', points: [{ t: 1, mantissa: 'invalid', expo: -2 }] })
      )
    )
    expect(await loadPriceHistory('Crypto.BTC/USD', 60_000)).toEqual([])
  })
})
