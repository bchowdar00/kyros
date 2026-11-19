import type { RustBookFrame } from '@kyros/domain'

import type { FeedState } from './feed'

export interface RustRound {
  marketId: number
  closesAt: number
}

/**
 * The Rust exchange re-sends its whole 20-level book on every change, so there is no
 * sequence to chain; the local counter only exists because `FeedState` asks for one.
 * `tickStep` is always one tick: every price on the grid is quotable.
 */
function level(entry: { price: number; qty: number }) {
  return { price: entry.price, qty: String(entry.qty), orders: 1 }
}

export function applyRustBook(previous: FeedState, frame: RustBookFrame): FeedState {
  return {
    ...previous,
    status: 'live',
    seq: String(BigInt(previous.seq ?? '0') + 1n),
    tickStep: 1,
    bids: frame.bids.map(level),
    asks: frame.asks.map(level),
  }
}

/** The round comes from `/ws/price`; the book has no market id or close time of its own. */
export function applyRustRound(
  previous: FeedState,
  round: RustRound | undefined,
  now: number
): FeedState {
  if (round === undefined) {
    return { ...previous, marketId: null, closesAt: null, state: null }
  }

  return {
    ...previous,
    marketId: String(round.marketId),
    closesAt: String(round.closesAt),
    state: now < round.closesAt ? 'open' : 'closed',
  }
}
