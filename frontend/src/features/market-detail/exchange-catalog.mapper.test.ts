import { describe, expect, it } from 'vitest'

import { resolvePriceMarket } from './active-market.mapper'
import {
  eventIdForMarket,
  marketIdForEvent,
  seriesIdForIndex,
  seriesIndexForId,
  toEventDetail,
  toSeriesRecord,
} from './exchange-catalog.mapper'

const open = {
  series: 'btc-2m',
  duration_secs: 120,
  current_market_id: 7,
  opens_at_ms: 1_788_598_800_000,
  closes_at_ms: 1_788_598_920_000,
}

describe('exchange catalog mapper', () => {
  it('offsets exchange ids into the EntityId range and back', () => {
    expect(seriesIndexForId(seriesIdForIndex(1))).toBe(1)
    expect(marketIdForEvent(eventIdForMarket(7))).toBe(7)
    expect(seriesIdForIndex(0)).toBeGreaterThanOrEqual(100_001)
  })

  it('names the series from its round length', () => {
    expect(toSeriesRecord(open, 100_001).title).toBe('BTC Up or Down 2m')
    expect(toSeriesRecord({ ...open, duration_secs: 900 }, 100_002).frequency).toBe('15m')
  })

  it('produces an event the price mapper accepts, with the strike left to the stream', () => {
    const detail = toEventDetail(open, 100_001)
    expect(detail?.id).toBe(eventIdForMarket(7))
    expect(detail?.startsAt).toBe('2026-09-05T09:00:00.000Z')
    const resolved = detail === undefined ? undefined : resolvePriceMarket(detail)
    expect(resolved?.priceChart.priceSymbol).toBe('btc-2m')
    expect(resolved?.priceChart.targetPrice).toBeNaN()
    expect(resolved?.priceChart.range).toEqual({ start: open.opens_at_ms, end: open.closes_at_ms })
  })

  it('skips a series whose first round has not opened', () => {
    expect(
      toEventDetail(
        { ...open, current_market_id: null, opens_at_ms: null, closes_at_ms: null },
        100_001
      )
    ).toBeUndefined()
  })
})
