import { describe, expect, it } from 'vitest'

import { resolvePriceMarket } from './active-market.mapper'
import { isNbaSeries, toEventDetail, toSeriesRecord } from './exchange-catalog.mapper'

const nba = {
  series: 'nba-h2h',
  duration_secs: 300,
  underlying: 'nba',
  outcomes: ['home', 'away'],
  current_market_id: 9,
  opens_at_ms: 1_788_598_800_000,
  closes_at_ms: 1_788_599_100_000,
  reference: { home_team: 'BOS', away_team: 'NYK', pregame_home_prob: 0.6313, game_seq: 1 },
}

describe('exchange catalog mapper for an NBA series', () => {
  it('tells an NBA series from a BTC one', () => {
    expect(isNbaSeries(nba)).toBe(true)
    expect(isNbaSeries({ underlying: 'btc' })).toBe(false)
    expect(isNbaSeries({})).toBe(false)
  })

  it('records the series as NBA head-to-head', () => {
    const record = toSeriesRecord(nba, 100_003)
    expect(record.assetType).toBe('nba')
    expect(record.marketType).toBe('h2h')
    expect(record.title).toBe('NBA Head-to-Head')
    expect(record.frequency).toBe('5m')
  })

  it('names the event after the matchup with the teams as the outcome labels', () => {
    const detail = toEventDetail(nba, 100_003)
    expect(detail?.title).toBe('Knicks at Celtics')
    const market = detail?.markets[0]
    expect(market?.yesLabel).toBe('BOS')
    expect(market?.noLabel).toBe('NYK')
    expect(market?.question).toBe('Will the Celtics beat the Knicks at home?')
    expect(market?.metadata.automation?.kind).toBe('nba_h2h')
    expect(market?.metadata.automation?.reference.mantissa).toBe('6313')
    const resolved = detail === undefined ? undefined : resolvePriceMarket(detail)
    expect(resolved?.priceChart.range).toEqual({ start: nba.opens_at_ms, end: nba.closes_at_ms })
  })

  it('falls back to the BTC shape when the round carries no matchup', () => {
    const detail = toEventDetail({ ...nba, reference: { strike_btc_usd: 1 } }, 100_003)
    expect(detail?.markets[0]?.yesLabel).toBe('UP')
  })
})
