import { describe, expect, it } from 'vitest'

import { initialFeed } from './feed'
import { applyRustBook, applyRustRound } from './rust-feed'

describe('rust feed adapter', () => {
  it('maps a book snapshot onto the exchange feed shape', () => {
    const feed = applyRustBook(initialFeed, {
      type: 'snapshot',
      series: 'btc-2m',
      bids: [{ price: 480, qty: 12 }],
      asks: [{ price: 500, qty: 7 }],
    })
    expect(feed.status).toBe('live')
    expect(feed.tickStep).toBe(1)
    expect(feed.seq).toBe('1')
    expect(feed.bids).toEqual([{ price: 480, qty: '12', orders: 1 }])
    expect(feed.asks).toEqual([{ price: 500, qty: '7', orders: 1 }])
    expect(
      applyRustBook(feed, { type: 'snapshot', series: 'btc-2m', bids: [], asks: [] }).seq
    ).toBe('2')
  })

  it('opens and closes the market from the round clock', () => {
    const round = { marketId: 7, closesAt: 1_000 }
    expect(applyRustRound(initialFeed, round, 999)).toMatchObject({
      marketId: '7',
      closesAt: '1000',
      state: 'open',
    })
    expect(applyRustRound(initialFeed, round, 1_000).state).toBe('closed')
    expect(applyRustRound(initialFeed, undefined, 0).marketId).toBeNull()
  })
})
