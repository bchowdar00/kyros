import type { RustMarket, RustTradeRow } from '@tiny/domain'
import { describe, expect, it } from 'vitest'

import {
  resolveBook,
  resolveCandles,
  resolveDistribution,
  resolveFills,
  resolveRounds,
} from './tables'

const me = `0x${'ab'.repeat(20)}`
const other = `0x${'cd'.repeat(20)}`
const identity = (address: string) => ({ address, root_address: address })
const trade = (ts: number, side: 'yes' | 'no', taker: string, maker: string): RustTradeRow => ({
  series: 'btc-2m',
  price: 620,
  qty: 5,
  ts,
  side,
  taker: identity(taker),
  maker: identity(maker),
})
const market = (
  id: number,
  strike: number,
  close: number | null,
  outcome: 'yes' | 'no' = 'yes'
): RustMarket => ({
  id,
  strike_btc_usd: strike,
  opened_at: id * 120,
  closes_at: id * 120 + 120,
  status:
    close === null ? { kind: 'open' } : { kind: 'resolved', outcome, close_price_btc_usd: close },
})

describe('table resolvers', () => {
  it('reads the book as cents with the requested depth', () => {
    const props = resolveBook(
      'btc-2m',
      {
        status: 'live',
        bids: [
          { price: 610, qty: 40 },
          { price: 600, qty: 20 },
        ],
        asks: [{ price: 630, qty: 10 }],
        marketId: 1,
        mid: 62,
        midPoints: [],
        receivedAt: 1,
      },
      1
    )
    expect(props).toEqual({
      series: 'btc-2m',
      asks: [{ price: 63, size: 10 }],
      bids: [{ price: 61, size: 40 }],
      rows: 1,
    })
  })

  it('reads the tape from the taker and my fills from my side of each trade', () => {
    const trades = [
      trade(3_000, 'no', other, me),
      trade(2_000, 'yes', me, other),
      trade(1_000, 'yes', other, other),
    ]
    const tape = resolveFills('btc-2m', trades, { kind: 'tape' }, 2, 'UTC')
    expect(tape.fills.map(fill => [fill.side, fill.priceCents])).toEqual([
      ['NO', 38],
      ['YES', 62],
    ])
    expect(tape.fills[0]?.time).toBe('00:00:03')
    const mine = resolveFills('btc-2m', trades, { kind: 'mine', address: me }, 10, 'UTC')
    // I made the first trade's NO buy, so I bought YES; I took the second as a YES buy.
    expect(mine.fills.map(fill => fill.side)).toEqual(['YES', 'YES'])
  })

  it('turns resolved rounds into outcomes oldest first and into a distribution', () => {
    const markets = [
      market(4, 100, null),
      market(3, 100, 130),
      market(2, 100, 90, 'no'),
      market(1, 100, 105),
    ]
    const rounds = resolveRounds('btc-2m', markets, 2)
    expect(rounds.rounds).toEqual([
      { id: 'round-2', outcome: 'down', moveDollars: -10 },
      { id: 'round-3', outcome: 'up', moveDollars: 30 },
    ])
    const distribution = resolveDistribution('btc-2m', markets, 10, 12)
    expect(distribution.values).toEqual([5, -10, 30])
    expect(distribution.current).toEqual({ value: 12, label: 'Now' })
    expect(distribution.format(-10)).toBe('-$10.00')
  })

  it('buckets spot into candles over the window with the strike as the guide', () => {
    const points = Array.from({ length: 60 }, (_, index) => ({
      time: index * 1_000,
      value: 100 + index,
    }))
    const props = resolveCandles(points, {
      windowMs: 30_000,
      bucketMs: 10_000,
      strike: 120,
      timeZone: 'UTC',
    })
    // Every bucket in the buffer is a candle; the widget clips to its window.
    expect(props.candles.map(candle => candle.time)).toEqual([
      0, 10_000, 20_000, 30_000, 40_000, 50_000,
    ])
    expect(props.candles[2]).toMatchObject({ open: 120, close: 129, low: 120, high: 129 })
    expect(props.bucketMs).toBe(10_000)
    expect(props.windowMs).toBe(30_000)
    expect(props.guide).toEqual({ value: 120, label: 'Strike' })
  })
})
