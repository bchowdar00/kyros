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
  series: 'btc-5m',
  duration_secs: 300,
  current_market_id: 7,
  opens_at_ms: 1_788_598_800_000,
  closes_at_ms: 1_788_599_100_000,
}

const nbaOpen = {
  series: 'nba-h2h',
  duration_secs: 300,
  underlying: 'nba',
  outcomes: ['home', 'away'],
  current_market_id: 3,
  opens_at_ms: 1_795_060_472_000,
  closes_at_ms: 1_795_069_800_000,
  reference: { home_team: 'DEN', away_team: 'BOS', pregame_home_prob: 0.5, game_seq: 1 },
}

describe('exchange catalog mapper', () => {
  it('offsets exchange ids into the EntityId range and back', () => {
    expect(seriesIndexForId(seriesIdForIndex(1))).toBe(1)
    expect(marketIdForEvent(eventIdForMarket(7))).toBe(7)
    expect(seriesIdForIndex(0)).toBeGreaterThanOrEqual(100_001)
  })

  it('names the series from its round length', () => {
    expect(toSeriesRecord(open, 100_001).title).toBe('BTC Up or Down 5m')
    expect(toSeriesRecord({ ...open, duration_secs: 900 }, 100_002).frequency).toBe('15m')
  })

  it('produces an event the price mapper accepts, with the strike left to the stream', () => {
    const detail = toEventDetail(open, 100_001)
    expect(detail?.id).toBe(eventIdForMarket(7))
    expect(detail?.startsAt).toBe('2026-09-05T09:00:00.000Z')
    const resolved = detail === undefined ? undefined : resolvePriceMarket(detail)
    expect(resolved?.priceChart.priceSymbol).toBe('btc-5m')
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

  it('names an NBA series as a game winner, with no round frequency', () => {
    const record = toSeriesRecord(nbaOpen, 100_003)
    expect(record.assetType).toBe('nba')
    expect(record.marketType).toBe('h2h')
    expect(record.frequency).toBeUndefined()
    expect(record.title).toBe('NBA game winner')
  })

  it('builds the NBA event with the two teams as outcomes', () => {
    const detail = toEventDetail(nbaOpen, 100_003)
    expect(detail?.title).toBe('BOS at DEN')
    expect(detail?.id).toBe(eventIdForMarket(3))
    const market = detail?.markets[0]
    expect(market?.yesLabel).toBe('DEN')
    expect(market?.noLabel).toBe('BOS')
    expect(market?.title).toBe('BOS at DEN')
    expect(market?.question).toBe('Which team wins? DEN is at home.')
    expect(market?.slug.endsWith('-nba-h2h')).toBe(true)
    expect(market?.metadata.automation?.kind).toBe('nba_h2h')
    expect(market?.metadata.automation?.priceSymbol).toBe('nba-h2h')
    expect(detail?.startsAt).toBe('2026-11-19T03:54:32.000Z')
  })

  it('skips an NBA event whose round fields or matchup are missing', () => {
    expect(
      toEventDetail(
        { ...nbaOpen, current_market_id: null, opens_at_ms: null, closes_at_ms: null },
        100_003
      )
    ).toBeUndefined()
    const { reference: _reference, ...noReference } = nbaOpen
    expect(toEventDetail(noReference, 100_003)).toBeUndefined()
    expect(
      toEventDetail({ ...noReference, reference: { strike_btc_usd: 1 } }, 100_003)
    ).toBeUndefined()
  })

  it('fails closed on an unknown underlying', () => {
    expect(toEventDetail({ ...nbaOpen, underlying: 'eth' }, 100_003)).toBeUndefined()
  })
})
