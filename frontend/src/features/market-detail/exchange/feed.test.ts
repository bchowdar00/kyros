import { expect, it } from 'vitest'
import { applyFeed, initialFeed } from './feed'
const live = applyFeed(initialFeed, {
  type: 'snapshot',
  seq: '1',
  market_id: '7',
  state: 'open',
  tick_step: 1,
  bids: [{ price: 500, qty: '2', orders: 1 }],
  asks: [],
  trades: [],
  closes_at_ms: null,
})
it('replaces absolute depth, removes empty levels, and refuses sequence gaps', () => {
  const changed = applyFeed(live, {
    type: 'delta',
    kind: 'book',
    seq: '2',
    market_id: '7',
    side: 'bid',
    price: 500,
    qty: '3',
    orders: 2,
  })
  expect(changed.bids[0]?.qty).toBe('3')
  const removed = applyFeed(changed, {
    type: 'delta',
    kind: 'book',
    seq: '3',
    market_id: '7',
    side: 'bid',
    price: 500,
    qty: '0',
    orders: 0,
  })
  expect(removed.bids).toEqual([])
  expect(applyFeed(live, { type: 'stale', seq: '4', market_id: '7' }).status).toBe('stale')
})
it('ignores predecessor lifecycle changes and keeps stale data gated until a snapshot', () => {
  const other = applyFeed(live, {
    type: 'delta',
    kind: 'lifecycle',
    seq: '2',
    market_id: '6',
    state: 'settled',
    tick_step: 1,
  })
  expect(other.state).toBe('open')
  const stale = applyFeed(other, { type: 'stale', seq: '3', market_id: '6' })
  expect(stale.status).toBe('stale')
  expect(applyFeed(stale, { type: 'delta', kind: 'stats', seq: '4', market_id: '7' }).status).toBe(
    'stale'
  )
  const healed = applyFeed(stale, {
    type: 'snapshot',
    seq: '5',
    market_id: '8',
    state: 'open',
    tick_step: 10,
    bids: [],
    asks: [],
    trades: [],
    closes_at_ms: null,
  })
  expect(healed.status).toBe('live')
  expect(healed.marketId).toBe('8')
  expect(healed.bids).toEqual([])
})
