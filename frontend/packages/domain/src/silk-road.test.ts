import { describe, expect, it } from 'vitest'
import * as z from 'zod/mini'

import { EventDetailSchema, SeriesListPageSchema } from './silk-road'

const activeEvent = {
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
            exponent: -8,
            instant: '2026-09-05T08:30:00Z',
            mantissa: '7961600000000',
            publishTimeMicros: '1788597000000000',
          },
          seriesId: 100_001,
          workflowId: 'market-cycle:100001:1788597000',
        },
      },
      noLabel: 'DOWN',
      question:
        'Will the BTC price at 2026-09-05T08:32:00Z be >= its price at 2026-09-05T08:30:00Z?',
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
  title: 'BTC price direction: 2026-09-05T08:30:00Z to 2026-09-05T08:32:00Z',
}

describe('EventDetailSchema', () => {
  it('parses a nested active Event DTO', () => {
    const parsed = z.parse(EventDetailSchema, activeEvent)

    expect(parsed.markets).toHaveLength(1)
    expect(parsed.markets[0]?.metadata.automation?.priceSymbol).toBe('Crypto.BTC/USD')
    expect(parsed.markets[0]?.metadata.automation?.reference.mantissa).toBe('7961600000000')
  })

  it('rejects a float mantissa', () => {
    expect(
      z.safeParse(EventDetailSchema, {
        ...activeEvent,
        markets: [
          {
            ...activeEvent.markets[0],
            metadata: {
              automation: {
                ...activeEvent.markets[0]?.metadata.automation,
                reference: {
                  exponent: -8,
                  instant: '2026-09-05T08:30:00Z',
                  mantissa: '79.616',
                  publishTimeMicros: '1788597000000000',
                },
              },
            },
          },
        ],
      }).success
    ).toBe(false)
  })
})

describe('SeriesListPageSchema', () => {
  it('parses a cursor page of Series', () => {
    const parsed = z.parse(SeriesListPageSchema, {
      hasMore: false,
      items: [
        {
          assetType: 'btc',
          frequency: '2m',
          id: 100_001,
          marketType: 'updown',
          slug: 'btc-updown-2m',
          status: 'SERIES_STATUS_ACTIVE',
          title: 'BTC 2-minute',
        },
      ],
    })

    expect(parsed.items[0]?.slug).toBe('btc-updown-2m')
  })
})
