import { describe, expect, it } from 'vitest'

import { kalshiHomeMarket, toKalshiHistory, toKalshiSparks } from './kalshi-history'

// Shapes read from Kalshi's public API on 2026-11-20 (the Toronto at Detroit game).
const candle = (endSeconds: number, bid: string, ask: string) => ({
  end_period_ts: endSeconds,
  price: { close_dollars: ask },
  volume_fp: '8.17',
  yes_ask: { close_dollars: ask },
  yes_bid: { close_dollars: bid },
})

const events = {
  events: [
    {
      event_ticker: 'KXNBAGAME-26NOV20CHAATL',
      markets: [{ ticker: 'KXNBAGAME-26NOV20CHAATL-ATL' }],
    },
    {
      event_ticker: 'KXNBAGAME-26NOV20TORDET',
      markets: [
        { ticker: 'KXNBAGAME-26NOV20TORDET-TOR' },
        { ticker: 'KXNBAGAME-26NOV20TORDET-DET' },
      ],
    },
  ],
}

describe('kalshiHomeMarket', () => {
  it("finds the home side's market of the game between two teams", () => {
    expect(kalshiHomeMarket(events, { away: 'TOR', home: 'DET' })).toBe(
      'KXNBAGAME-26NOV20TORDET-DET'
    )
  })

  it('has no market for a game Kalshi does not list, or a body it cannot read', () => {
    expect(kalshiHomeMarket(events, { away: 'DET', home: 'TOR' })).toBeUndefined()
    expect(kalshiHomeMarket({ events: 'nope' }, { away: 'TOR', home: 'DET' })).toBeUndefined()
    expect(kalshiHomeMarket(null, { away: 'TOR', home: 'DET' })).toBeUndefined()
  })
})

describe('toKalshiHistory', () => {
  it('reads each minute as the middle of its closing bid and ask, oldest first', () => {
    const body = {
      candlesticks: [
        candle(1_795_148_700, '0.7200', '0.7400'),
        candle(1_795_148_640, '0.72', '0.73'),
      ],
    }

    expect(toKalshiHistory(body)).toEqual([
      { time: 1_795_148_640_000, value: 0.725 },
      { time: 1_795_148_700_000, value: 0.73 },
    ])
  })

  it('skips a minute with no two-sided price, a crossed book or one too wide to call', () => {
    const body = {
      candlesticks: [
        { end_period_ts: 1_795_148_640, yes_ask: {}, yes_bid: { close_dollars: '0.72' } },
        candle(1_795_148_700, '0.75', '0.73'),
        candle(1_795_148_760, '0.10', '0.80'),
        candle(1_795_148_820, '0', '0.01'),
        candle(1_795_148_880, '0.70', '0.72'),
        { junk: true },
      ],
    }

    expect(toKalshiHistory(body)).toEqual([{ time: 1_795_148_880_000, value: 0.71 }])
  })

  it('is empty for a body that is not the expected shape, and never throws', () => {
    expect(toKalshiHistory(null)).toEqual([])
    expect(toKalshiHistory({})).toEqual([])
    expect(toKalshiHistory({ candlesticks: 'nope' })).toEqual([])
  })
})

describe('toKalshiSparks', () => {
  it('gives each market its own run of prices, oldest first', () => {
    const body = {
      markets: [
        {
          candlesticks: [
            candle(1_795_100_000, '0.30', '0.32'),
            candle(1_795_103_600, '0.34', '0.36'),
          ],
          market_ticker: 'A',
        },
        { candlesticks: [candle(1_795_100_000, '0.10', '0.90')], market_ticker: 'B' },
        { junk: true },
      ],
    }

    expect(toKalshiSparks(body)).toEqual({
      A: [
        { time: 1_795_100_000_000, value: 0.31 },
        { time: 1_795_103_600_000, value: 0.35 },
      ],
      B: [],
    })
    expect(toKalshiSparks(null)).toEqual({})
  })
})
