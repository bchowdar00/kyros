import type { EventDetail, SeriesRecord } from '@tiny/domain'
import { describe, expect, it } from 'vitest'

import { toHomeMarketCard, toMarketDetail } from './event-detail.mapper'
import { detail, series } from './live-event.fixture'

const nbaSeries: SeriesRecord = {
  ...series,
  assetType: 'nba',
  marketType: 'h2h',
  slug: 'nba-h2h',
  title: 'NBA Head-to-Head',
}

const nbaDetail: EventDetail = {
  ...detail,
  markets: detail.markets.map(market => ({
    ...market,
    metadata: {
      automation:
        market.metadata.automation === undefined
          ? undefined
          : { ...market.metadata.automation, kind: 'nba_h2h', priceSymbol: 'nba-h2h' },
    },
    noLabel: 'NYK',
    yesLabel: 'BOS',
    question: 'Will the Celtics beat the Knicks at home?',
    title: 'Knicks at Celtics',
  })),
}

describe('event detail mapper for an NBA round', () => {
  it('renders the matchup with a probability chart, teams and no strike chart', () => {
    const market = toMarketDetail(nbaSeries, nbaDetail)
    expect(market?.category).toBe('NBA')
    expect(market?.header.title).toBe('Knicks at Celtics')
    expect(market?.header.outcomes).toEqual([
      { label: 'NYK', side: 'away' },
      { label: 'BOS', side: 'home' },
    ])
    expect(market?.header.teams?.[1].name).toBe('Boston Celtics')
    expect(market?.header.teams?.[0].logo.src).toBe('/assets/market/teams/new-york-knicks.svg')
    expect(market?.priceChart).toBeUndefined()
    expect(market?.probabilityChart?.series[0]).toEqual({ label: 'BOS', points: [], side: 'home' })
    expect(market?.orderBook?.columnLabels[0]).toBe('Price (HOME)')
  })

  it('renders the home card with the two teams and a pregame status', () => {
    const card = toHomeMarketCard(nbaSeries, nbaDetail)
    expect(card?.category).toBe('NBA')
    expect(card?.title).toBe('Knicks at Celtics')
    expect(card?.marketStatus).toBe('Pregame')
    expect(card?.priceChart).toBeUndefined()
    expect(card?.series?.[1].side).toBe('away')
  })

  it('leaves a BTC series exactly as it was', () => {
    expect(toMarketDetail(series, detail)?.category).toBe('Crypto')
    expect(toHomeMarketCard(series, detail)?.priceChart).toBeDefined()
  })
})
