import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'

import { createSilkRoadClient } from './silk-road'

const server = setupServer()
const noSleep = () => Promise.resolve()

beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

const eventDetail = {
  endsAt: '2026-09-05T08:32:00Z',
  id: 100_002,
  markets: [
    {
      endsAt: '2026-09-05T08:32:00Z',
      eventId: 100_002,
      id: 100_003,
      metadata: {
        automation: {
          kind: 'btc_updown',
          priceSymbol: 'Crypto.BTC/USD',
          reference: {
            exponent: 0,
            instant: '2026-09-05T08:30:00Z',
            mantissa: '79616',
            publishTimeMicros: '1788597000000000',
          },
          seriesId: 100_001,
          workflowId: 'market-cycle:100001:1788597000',
        },
      },
      noLabel: 'DOWN',
      question: 'Will BTC finish up?',
      slug: 'market-cycle-100001-1788597000-btc-updown',
      startsAt: '2026-09-05T08:30:00Z',
      status: 'MARKET_STATUS_ACTIVE',
      title: 'BTC price: UP or DOWN',
      yesLabel: 'UP',
    },
  ],
  seriesId: 100_001,
  slug: 'market-cycle-100001-1788597000',
  startsAt: '2026-09-05T08:30:00Z',
  status: 'EVENT_STATUS_ACTIVE',
  title: 'BTC price direction',
}

const { markets, ...eventSummary } = eventDetail

function mockMarkets(): void {
  server.use(
    http.get('http://silk.test/v1/markets', ({ request }) => {
      expect(new URL(request.url).searchParams.get('eventId')).toBe('100002')
      return HttpResponse.json({ hasMore: false, items: markets })
    })
  )
}

function client() {
  return createSilkRoadClient({ baseUrl: 'http://silk.test', sleep: noSleep })
}

describe('createSilkRoadClient', () => {
  it('composes an Event and its Markets by id', async () => {
    server.use(
      http.get('http://silk.test/v1/events', () =>
        HttpResponse.json({ hasMore: false, items: [eventSummary] })
      )
    )
    mockMarkets()

    const result = await client().getEvent(100_002)

    expect(result.ok).toBe(true)
    if (result.ok) {
      expect(result.data.id).toBe(100_002)
      expect(result.data.markets[0]?.metadata.automation?.priceSymbol).toBe('Crypto.BTC/USD')
    }
  })

  it('reads all event pages and composes the newest active Event for a Series', async () => {
    server.use(
      http.get('http://silk.test/v1/events', ({ request }) => {
        const searchParams = new URL(request.url).searchParams
        expect(searchParams.get('seriesId')).toBe('100001')

        return searchParams.get('cursor') === null
          ? HttpResponse.json({
              hasMore: true,
              items: [{ ...eventSummary, id: 100_001, startsAt: '2026-09-05T08:28:00Z' }],
              nextCursor: 100_001,
            })
          : HttpResponse.json({ hasMore: false, items: [eventSummary] })
      })
    )
    mockMarkets()

    const result = await client().getActiveEvent(100_001)

    expect(result.ok).toBe(true)
    if (result.ok) {
      expect(result.data.id).toBe(100_002)
    }
  })

  it('reports a missing active Event as HTTP 404', async () => {
    server.use(
      http.get('http://silk.test/v1/events', () => HttpResponse.json({ hasMore: false, items: [] }))
    )

    const result = await client().getActiveEvent(100_001)

    expect(result.ok).toBe(false)
    if (!result.ok) {
      expect(result.error.status).toBe(404)
    }
  })
})
