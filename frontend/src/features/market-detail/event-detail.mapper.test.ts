import type { RustGeminiSeries } from '@kyros/domain'
import { describe, expect, it } from 'vitest'

import { detail, series } from './live-event.fixture'

import { headerCountdownRange, toActiveMarketIdentity } from './active-market.mapper'
import { toEventDetail, toSeriesRecord } from './exchange-catalog.mapper'
import { toHomeMarketCard, toMarketDetail } from './event-detail.mapper'
import { getMarketDetailFixture } from './market-detail.fixture'
import { PUBLIC_STRATEGY_FIXTURES } from './public-strategies.fixture'

const nbaInfo = {
  series: 'nba-h2h',
  duration_secs: 300,
  underlying: 'nba',
  outcomes: ['home', 'away'],
  current_market_id: 3,
  opens_at_ms: 1_795_060_472_000,
  closes_at_ms: 1_795_069_800_000,
  reference: { home_team: 'DEN', away_team: 'BOS', pregame_home_prob: 0.5, game_seq: 1 },
}
const nbaSeries = toSeriesRecord(nbaInfo, 100_003)
const nbaDetail = toEventDetail(nbaInfo, 100_003)
if (nbaDetail === undefined) throw new Error('nba fixture must build an event')
const nbaGemini: RustGeminiSeries = {
  env: 'production',
  series: 'NBA',
  phase: 'open',
  book: 'live',
  starts_at_ms: 1_795_055_400_000,
}

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

    expect(card?.title).toBe('BTC Up or Down 5m')
    expect(market?.header.title).toBe('BTC Up or Down 5m')
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

  it('exposes an NBA rollover identity without a price chart', () => {
    expect(toActiveMarketIdentity(nbaDetail)).toEqual({ eventId: nbaDetail.id })
  })

  it('maps an NBA event to a sports probability market', () => {
    const market = toMarketDetail(nbaSeries, nbaDetail, undefined, nbaGemini)

    expect(market?.category).toBe('Sports')
    expect(market?.header.title).toBe('BOS at DEN')
    expect(market?.header.subtitle).toBe('Which team wins? DEN is at home.')
    expect(market?.header.outcomes).toEqual([
      { label: 'DEN', side: 'home' },
      { label: 'BOS', side: 'away' },
    ])
    expect(market?.priceChart).toBeUndefined()
    expect(market?.probabilityChart?.live).toBe(true)
    expect(market?.probabilityChart?.series.map(entry => entry.label)).toEqual(['DEN', 'BOS'])
    expect(market?.probabilityChart?.seriesSlug).toBe('nba-h2h')
    expect(market?.probabilityChart?.range).toEqual({
      start: nbaInfo.opens_at_ms,
      end: nbaInfo.closes_at_ms,
    })
    expect(market?.orderBook?.columnLabels).toEqual(['Price (DEN)', 'Lots', 'Total'])
    expect(market?.signals).toBeDefined()
    expect(market?.myStrategies).toBeDefined()
    expect(market?.scoreboard).toEqual({ home: 'DEN', away: 'BOS', tipoffMs: 1_795_055_400_000 })
  })

  it('leaves out the tipoff when the series follows no Gemini window', () => {
    const market = toMarketDetail(nbaSeries, nbaDetail)
    expect(market?.scoreboard).toEqual({ home: 'DEN', away: 'BOS' })
  })

  it('builds an NBA home card with two empty series and no price chart', () => {
    const card = toHomeMarketCard(nbaSeries, nbaDetail)

    expect(card?.category).toBe('Sports')
    expect(card?.title).toBe('BOS at DEN')
    expect(card?.subtitle).toBe('Which team wins? DEN is at home.')
    expect(card?.priceChart).toBeUndefined()
    expect(card?.live).toBe(true)
    expect(card?.series).toEqual([
      { label: 'DEN', side: 'home', points: [] },
      { label: 'BOS', side: 'away', points: [] },
    ])
    expect(card?.outcomes).toEqual([
      { label: 'DEN', side: 'home' },
      { label: 'BOS', side: 'away' },
    ])
  })

  it('counts a strike market and a plain probability market down to their close', () => {
    const strike = toMarketDetail(series, detail)
    expect(strike?.priceChart?.range).toBeDefined()
    expect(strike && headerCountdownRange(strike)).toEqual(strike?.priceChart?.range)

    const probabilityChart = getMarketDetailFixture('btc-above-118k-0903')?.probabilityChart
    if (probabilityChart === undefined) throw new Error('the fixture carries a probability chart')
    expect(headerCountdownRange({ probabilityChart })).toEqual(probabilityChart.range)
  })

  it('shows no countdown for a game, which ends at its final and not at a time', () => {
    const game = toMarketDetail(nbaSeries, nbaDetail, undefined, nbaGemini)
    expect(game?.probabilityChart?.range).toBeDefined()
    expect(game && headerCountdownRange(game)).toBeUndefined()
  })
})
