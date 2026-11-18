import { describe, expect, it } from 'vitest'

import { detail, series } from './live-event.fixture'

import { toActiveMarketIdentity } from './active-market.mapper'
import { toHomeMarketCard, toMarketDetail } from './event-detail.mapper'
import { PUBLIC_STRATEGY_FIXTURES } from './public-strategies.fixture'

describe('event detail mapper', () => {
  it('exposes a rollover identity only once the market can render', () => {
    expect(toActiveMarketIdentity(detail)).toEqual({ eventId: detail.id })
    expect(toActiveMarketIdentity({ ...detail, markets: [] })).toBeUndefined()
    expect(toActiveMarketIdentity({ ...detail, endsAt: detail.startsAt })).toBeUndefined()
    expect(toMarketDetail(series, detail)?.liveEvent).toEqual({
      eventId: detail.id,
      seriesId: series.id,
    })
  })

  it('uses the Series name instead of the timestamped Event title', () => {
    const card = toHomeMarketCard(series, detail)
    const market = toMarketDetail(series, detail)

    expect(card?.title).toBe('BTC Up or Down 2m')
    expect(market?.header.title).toBe('BTC Up or Down 2m')
    expect(card?.title).not.toContain('2026-09-05')
    expect(market?.header.title).not.toContain('2026-09-05')
  })

  it('keeps the market screen chrome next to the live chart', () => {
    const market = toMarketDetail(series, detail)

    expect(market?.signals).toBeDefined()
    expect(market?.orderBook?.bids).toEqual([])
    expect(market?.header.outcomes.every(outcome => outcome.priceCents === undefined)).toBe(true)
    expect(market?.myStrategies).toBeDefined()
    expect(market?.priceChart?.targetPrice).toBe(79_650)
  })

  it('shows the exchange catalogue as the strategies column and fails closed without one', () => {
    expect(toMarketDetail(series, detail, PUBLIC_STRATEGY_FIXTURES)?.chatPanel).toEqual({
      publicStrategies: PUBLIC_STRATEGY_FIXTURES,
      availability: 'ready',
      view: 'strategies',
    })
    expect(toMarketDetail(series, detail)?.chatPanel).toEqual({
      publicStrategies: [],
      availability: 'unavailable',
      view: 'strategies',
    })
  })
})
