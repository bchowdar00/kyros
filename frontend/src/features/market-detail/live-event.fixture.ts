import type { EventDetail, SeriesRecord } from '@tiny/domain'

export const series: SeriesRecord = {
  assetType: 'btc',
  frequency: '2m',
  id: 100_001,
  marketType: 'updown',
  slug: 'btc-updown-2m',
  status: 'SERIES_STATUS_ACTIVE',
}

export const detail: EventDetail = {
  endsAt: '2026-09-05T09:02:00Z',
  id: 100_002,
  markets: [
    {
      endsAt: '2026-09-05T09:02:00Z',
      eventId: 100_002,
      id: 100_003,
      metadata: {
        automation: {
          kind: 'btc_updown',
          priceSymbol: 'Crypto.BTC/USD',
          reference: {
            exponent: 0,
            instant: '2026-09-05T09:00:00Z',
            mantissa: '79650',
            publishTimeMicros: '1788598800000000',
          },
          seriesId: 100_001,
          workflowId: 'market-cycle:100001:1788598800',
        },
      },
      noLabel: 'DOWN',
      question:
        'Will the BTC price at 2026-09-05T09:02:00Z be >= its price at 2026-09-05T09:00:00Z?',
      slug: 'market-cycle-100001-1788598800-btc-updown',
      startsAt: '2026-09-05T09:00:00Z',
      status: 'MARKET_STATUS_ACTIVE',
      title: 'BTC price: UP or DOWN',
      yesLabel: 'UP',
    },
  ],
  seriesId: 100_001,
  slug: 'market-cycle-100001-1788598800',
  startsAt: '2026-09-05T09:00:00Z',
  status: 'EVENT_STATUS_ACTIVE',
  title: 'BTC price direction: 2026-09-05T09:00:00Z to 2026-09-05T09:02:00Z',
}
