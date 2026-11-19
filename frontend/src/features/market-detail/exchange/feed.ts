import type { ExchangeFeed, ExchangeLevel } from '@kyros/domain'

export interface FeedState {
  status: 'connecting' | 'live' | 'stale' | 'disconnected' | 'paused'
  seq: string | null
  marketId: string | null
  state: string | null
  tickStep: number | null
  closesAt: string | null
  bids: ExchangeLevel[]
  asks: ExchangeLevel[]
}
export const initialFeed: FeedState = {
  status: 'connecting',
  seq: null,
  marketId: null,
  state: null,
  tickStep: null,
  closesAt: null,
  bids: [],
  asks: [],
}

export function applyFeed(previous: FeedState, frame: ExchangeFeed): FeedState {
  if (frame.type === 'snapshot')
    return {
      status: 'live',
      seq: frame.seq,
      marketId: frame.market_id,
      state: frame.state,
      tickStep: frame.tick_step,
      closesAt: frame.closes_at_ms,
      bids: frame.bids,
      asks: frame.asks,
    }
  if (previous.seq === null || BigInt(frame.seq) !== BigInt(previous.seq) + 1n) {
    return { ...previous, status: 'stale', seq: null }
  }
  const next = { ...previous, seq: frame.seq }
  if (frame.type === 'stale') return { ...next, status: 'stale' }
  if (frame.market_id !== previous.marketId) return next
  if (frame.kind === 'lifecycle') return { ...next, state: frame.state, tickStep: frame.tick_step }
  if (frame.kind === 'tick_step_change') return { ...next, tickStep: frame.new_tick_step }
  if (frame.kind !== 'book') return next
  const key = frame.side === 'bid' ? 'bids' : 'asks'
  const levels = previous[key].filter(level => level.price !== frame.price)
  if (frame.qty !== '0') levels.push({ price: frame.price, qty: frame.qty, orders: frame.orders })
  levels.sort((left, right) =>
    frame.side === 'bid' ? right.price - left.price : left.price - right.price
  )
  return { ...next, [key]: levels }
}
