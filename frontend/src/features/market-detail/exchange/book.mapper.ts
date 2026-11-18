import type { ExchangeLevel } from '@tiny/domain'

import type { MarketOutcome, OrderBookData, OrderBookLevel } from '../types'
import type { FeedState } from './feed'

const VISIBLE_LEVELS = 12

/**
 * Depth bars are cumulative from the spread outward, as on a depth chart: a level's bar
 * is the liquidity a taker would walk through to reach it, so bars only grow away from
 * the spread even when a single far level is thin. Both sides share one scale.
 */
function cumulative(levels: readonly ExchangeLevel[]): { qty: bigint; level: ExchangeLevel }[] {
  let running = 0n
  return levels.map(level => {
    running += BigInt(level.qty)
    return { qty: running, level }
  })
}

export function exchangeBook(feed: FeedState): OrderBookData {
  const asks = cumulative(feed.asks.slice(0, VISIBLE_LEVELS))
  const bids = cumulative(feed.bids.slice(0, VISIBLE_LEVELS))
  let maximum = 1n
  for (const entry of [...asks, ...bids]) if (entry.qty > maximum) maximum = entry.qty
  const map = (entries: typeof asks): OrderBookLevel[] =>
    entries.map(({ level, qty }) => ({
      priceTicks: level.price,
      qty: level.qty,
      totalMicros: String(BigInt(level.qty) * BigInt(level.price) * 1000n),
      depthPercentage: Number((qty * 10000n) / maximum) / 100,
    }))
  const bid = feed.bids[0]?.price
  const ask = feed.asks[0]?.price
  return {
    title: 'Order book',
    actionLabel: 'Center order book',
    columnLabels: ['Price (YES)', 'Lots', 'Total'],
    // Asks render top-down toward the spread, so the deepest cumulative bar sits on top.
    asks: map(asks).toReversed(),
    bids: map(bids),
    spreadLabel: 'Spread',
    spreadTicks: bid === undefined || ask === undefined ? null : ask - bid,
  }
}
/**
 * The grid's extreme ticks, in tenths of a cent. A side with nothing resting has been bought
 * all the way up: no asks means YES is at the top tick, no bids means NO is, so each outcome
 * still quotes 99.9¢ rather than nothing. Mirrors the engine's `implied_mark_yes_tenths`.
 */
export const TOP_TICK = 999
export const BOTTOM_TICK = 1

/** Best YES ask, or the top tick when the ask side is empty. */
export function bestAskOrTop(feed: Pick<FeedState, 'asks'>): number {
  return feed.asks[0]?.price ?? TOP_TICK
}

/** Best YES bid, or the bottom tick when the bid side is empty. */
export function bestBidOrBottom(feed: Pick<FeedState, 'bids'>): number {
  return feed.bids[0]?.price ?? BOTTOM_TICK
}

export function exchangeOutcomes(
  outcomes: readonly [MarketOutcome, MarketOutcome],
  feed: FeedState
): readonly [MarketOutcome, MarketOutcome] {
  const map = (outcome: MarketOutcome): MarketOutcome => ({
    label: outcome.label,
    side: outcome.side,
    ...(feed.status === 'live' && feed.state === 'open'
      ? {
          priceTicks: outcome.side === 'home' ? bestAskOrTop(feed) : 1000 - bestBidOrBottom(feed),
        }
      : {}),
  })
  return [map(outcomes[0]), map(outcomes[1])]
}
