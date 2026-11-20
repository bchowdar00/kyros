import { describe, expect, it } from 'vitest'

import {
  extendVenueHistory,
  venueLinesFor,
  venuePins,
  venueSignalIds,
  withVenuePast,
} from './venue-lines'

const T = Date.UTC(2026, 10, 20, 11, 0)

describe('venueLinesFor', () => {
  it("draws Polymarket's and Kalshi's price for the home side of the game, in percent", () => {
    const lines = venueLinesFor('nba-h2h', {
      'nba.live.polymarket.v1:home_prob': [
        { time: T, value: 0.61 },
        { time: T + 3000, value: 0.625 },
      ],
      'nba.live.kalshi.v1:home_prob': [
        { time: T, value: 0.6 },
        { time: T + 3000, value: 0.615 },
      ],
    })

    expect(lines).toEqual([
      {
        id: 'polymarket',
        label: 'Polymarket',
        points: [
          { time: T, value: 61 },
          { time: T + 3000, value: 62.5 },
        ],
      },
      {
        id: 'kalshi',
        label: 'Kalshi',
        points: [
          { time: T, value: 60 },
          { time: T + 3000, value: 61.5 },
        ],
      },
    ])
  })

  it("draws Polymarket's price for Up on the fifteen-minute Bitcoin round, its own window", () => {
    const lines = venueLinesFor('btc-15m', {
      'btc.polymarket.prediction_quote.v1:affirmative_midpoint': [
        { time: T, value: 0.5 },
        { time: T + 1000, value: 0.52 },
      ],
    })

    expect(lines.map(line => line.id)).toEqual(['polymarket'])
    expect(lines[0]?.points.at(-1)).toEqual({ time: T + 1000, value: 52 })
  })

  it('leaves out a venue with fewer than two readings or a reading that is not a price', () => {
    const lines = venueLinesFor('nba-h2h', {
      'nba.live.polymarket.v1:home_prob': [{ time: T, value: 0.61 }],
      'nba.live.kalshi.v1:home_prob': [
        { time: T, value: 0.6 },
        { time: T + 3000, value: 61 },
      ],
    })

    expect(lines).toEqual([])
  })

  it('names the signals to subscribe to for a series', () => {
    expect(venueSignalIds('nba-h2h')).toEqual(['nba.live.polymarket.v1', 'nba.live.kalshi.v1'])
    expect(venueSignalIds('eth-1h')).toEqual([])
  })

  it('draws no line on the five-minute round, which Polymarket does not price', () => {
    expect(venuePins('btc-5m')).toEqual([])
  })

  it('adds new readings to the ones already held and drops those past the window', () => {
    const pin = 'nba.live.kalshi.v1:home_prob'
    const held = {
      [pin]: [
        { time: T, value: 0.6 },
        { time: T + 60_000, value: 0.61 },
      ],
    }
    const incoming = {
      [pin]: [
        { time: T + 60_000, value: 0.61 },
        { time: T + 120_000, value: 0.62 },
      ],
      'other.signal:field': [{ time: T, value: 1 }],
    }

    expect(extendVenueHistory(held, incoming, 'nba-h2h', 90_000)).toEqual({
      'nba.live.polymarket.v1:home_prob': [],
      [pin]: [
        { time: T + 60_000, value: 0.61 },
        { time: T + 120_000, value: 0.62 },
      ],
    })
  })

  it('thins a long window as it fills, keeping the first reading and always the newest', () => {
    const pin = 'nba.live.kalshi.v1:home_prob'
    const threeHours = 3 * 60 * 60 * 1000
    let held = {}
    // A reading a second for ten minutes, arriving one at a time as they do on the page.
    for (let second = 0; second < 600; second += 1) {
      const reading = { time: T + second * 1000, value: 0.5 + second / 10_000 }
      held = extendVenueHistory(held, { [pin]: [reading] }, 'nba-h2h', threeHours)
    }

    const kept = (held as Record<string, { time: number; value: number }[]>)[pin] ?? []
    // Three hours drawn from at most 720 readings: one every fifteen seconds.
    expect(kept.length).toBeLessThanOrEqual(42)
    expect(kept.length).toBeGreaterThanOrEqual(38)
    expect(kept[0]).toEqual({ time: T, value: 0.5 })
    expect(kept.at(-1)).toEqual({ time: T + 599_000, value: 0.5 + 599 / 10_000 })
  })

  it('hands back the very same history when nothing new arrived', () => {
    const pin = 'nba.live.kalshi.v1:home_prob'
    const once = extendVenueHistory({}, { [pin]: [{ time: T, value: 0.6 }] }, 'nba-h2h', 90_000)

    expect(extendVenueHistory(once, { [pin]: [{ time: T, value: 0.6 }] }, 'nba-h2h', 90_000)).toBe(
      once
    )
  })

  it("opens a chart with a venue's past prices and stretches its window back to them", () => {
    const chart = { live: true, range: { end: T + 3 * 3_600_000, start: T } }
    const past = [
      { time: T - 7_200_000, value: 0.7 },
      { time: T - 3_600_000, value: 0.72 },
    ]

    const opened = withVenuePast(chart, 'nba-h2h', 'kalshi', past)

    expect(opened.range).toEqual({ end: T + 3 * 3_600_000, start: T - 7_200_000 })
    expect(opened.venueHistory).toEqual({ 'nba.live.kalshi.v1:home_prob': past })
    expect(opened.live).toBe(true)
    // The page then draws the line from the past and goes on from the live readings.
    const held = extendVenueHistory({}, opened.venueHistory ?? {}, 'nba-h2h', 15 * 3_600_000)
    expect(venueLinesFor('nba-h2h', held).map(line => line.id)).toEqual(['kalshi'])
  })

  it('leaves a chart as it is when there is no past to draw or no such venue', () => {
    const chart = { range: { end: T + 1, start: T } }

    expect(withVenuePast(chart, 'nba-h2h', 'kalshi', [{ time: T - 60_000, value: 0.7 }])).toBe(
      chart
    )
    expect(withVenuePast(chart, 'nba-h2h', 'kalshi', undefined)).toBe(chart)
    expect(
      withVenuePast(chart, 'btc-5m', 'kalshi', [
        { time: T - 2, value: 0.7 },
        { time: T - 1, value: 0.7 },
      ])
    ).toBe(chart)
  })

  it('knows no venue for a series it has no line for', () => {
    expect(venuePins('eth-1h')).toEqual([])
    expect(venueLinesFor('eth-1h', {})).toEqual([])
  })
})
