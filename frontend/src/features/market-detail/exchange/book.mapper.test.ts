import { describe, expect, it } from 'vitest'

import type { MarketOutcome, OrderBookData } from '../types'
import { exchangeBook, exchangeOutcomes } from './book.mapper'
import { initialFeed } from './feed'

const level = (price: number, qty: number) => ({ price, qty: String(qty), orders: 1 })
const depth = (levels: OrderBookData['asks']) =>
  levels.map(entry => ('depthPercentage' in entry ? entry.depthPercentage : Number.NaN))

describe('exchangeBook', () => {
  it('makes depth bars cumulative away from the spread on one shared scale', () => {
    const book = exchangeBook({
      ...initialFeed,
      status: 'live',
      // Best ask first; the far ask is thin but must still get the biggest ask bar.
      asks: [level(510, 50), level(520, 200), level(530, 10)],
      bids: [level(500, 100), level(490, 300)],
    })
    // Rendered top-down: farthest ask on top with the full cumulative 260 lots.
    expect(depth(book.asks)).toEqual([65, 62.5, 12.5])
    expect(depth(book.bids)).toEqual([25, 100])
    expect(book.spreadTicks).toBe(10)
  })
})

describe('exchangeOutcomes', () => {
  const outcomes: readonly [MarketOutcome, MarketOutcome] = [
    { label: 'UP', side: 'home' },
    { label: 'DOWN', side: 'away' },
  ]
  const open = { ...initialFeed, status: 'live' as const, state: 'open' }

  it('quotes YES from the best ask and NO from the complement of the best bid', () => {
    const [up, down] = exchangeOutcomes(outcomes, {
      ...open,
      asks: [level(520, 5)],
      bids: [level(490, 5)],
    })
    expect(up.priceTicks).toBe(520)
    expect(down.priceTicks).toBe(510)
  })

  it('quotes the top tick for a side that has been bought all the way up', () => {
    const [upNoAsks, downWithBids] = exchangeOutcomes(outcomes, {
      ...open,
      asks: [],
      bids: [level(600, 5)],
    })
    expect(upNoAsks.priceTicks).toBe(999)
    expect(downWithBids.priceTicks).toBe(400)
    const [upWithAsks, downNoBids] = exchangeOutcomes(outcomes, {
      ...open,
      asks: [level(300, 5)],
      bids: [],
    })
    expect(upWithAsks.priceTicks).toBe(300)
    expect(downNoBids.priceTicks).toBe(999)
  })

  it('quotes nothing while the feed is not live or the round is not open', () => {
    const stale = exchangeOutcomes(outcomes, { ...open, status: 'stale', asks: [level(520, 5)] })
    const closed = exchangeOutcomes(outcomes, { ...open, state: 'closed', asks: [level(520, 5)] })
    for (const [up, down] of [stale, closed]) {
      expect(up.priceTicks).toBeUndefined()
      expect(down.priceTicks).toBeUndefined()
    }
  })
})
