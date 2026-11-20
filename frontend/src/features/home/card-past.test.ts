import { describe, expect, it } from 'vitest'

import { joinCardLine, withCardPast } from './card-past'
import type { HomeMarketCardModel } from './types'

const game: HomeMarketCardModel = {
  category: 'Sports',
  live: true,
  outcomes: [
    { label: 'DET', side: 'home' },
    { label: 'TOR', side: 'away' },
  ],
  range: { end: 2000, start: 1000 },
  series: [
    { label: 'DET', points: [], side: 'home' },
    { label: 'TOR', points: [], side: 'away' },
  ],
  slug: 'nba-h2h',
  subtitle: 'Who wins?',
  timeZone: 'UTC',
  title: 'TOR at DET',
}

describe('withCardPast', () => {
  it("opens a game's lines with the home side's past price and what is left of a dollar", () => {
    const card = withCardPast(game, [
      { time: 100, value: 0.705 },
      { time: 200, value: 0.72 },
    ])

    expect(card.series?.[0].points).toEqual([
      { time: 100, value: 70.5 },
      { time: 200, value: 72 },
    ])
    expect(card.series?.[1].points).toEqual([
      { time: 100, value: 29.5 },
      { time: 200, value: 28 },
    ])
  })

  it('leaves the card alone without two past prices, or when it charts a price instead', () => {
    expect(withCardPast(game, undefined)).toBe(game)
    expect(withCardPast(game, [{ time: 100, value: 0.7 }])).toBe(game)

    const { series: _series, ...rest } = game
    const strike: HomeMarketCardModel = {
      ...rest,
      priceChart: { label: 'BTC', points: [], targetLabel: 'Strike', targetPrice: 1 },
    }
    expect(
      withCardPast(strike, [
        { time: 100, value: 0.7 },
        { time: 200, value: 0.8 },
      ])
    ).toBe(strike)
  })
})

describe('joinCardLine', () => {
  it('keeps the past that came before the live points and follows it with them', () => {
    const past = [
      { time: 100, value: 70 },
      { time: 200, value: 72 },
      { time: 300, value: 73 },
    ]
    const live = [
      { time: 250, value: 71 },
      { time: 400, value: 71.5 },
    ]

    expect(joinCardLine(past, live)).toEqual([
      { time: 100, value: 70 },
      { time: 200, value: 72 },
      { time: 250, value: 71 },
      { time: 400, value: 71.5 },
    ])
  })

  it('is the live points alone when there is no past, and the past alone before any arrive', () => {
    const live = [{ time: 250, value: 71 }]
    const past = [{ time: 100, value: 70 }]

    expect(joinCardLine([], live)).toEqual(live)
    expect(joinCardLine(past, [])).toEqual(past)
  })
})
