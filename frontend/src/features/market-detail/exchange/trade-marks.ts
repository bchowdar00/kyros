import type { RustUserTradeRow } from '@kyros/domain'
import { formatExchangeTicks } from '@kyros/money'
import type { TradeMark } from '@kyros/ui/charts/trade-marks'

import { cardNameOf, type StrategyCardName } from '../strategy-card-names'
import type { MarketOutcome } from '../types'
import { outcomeLabel } from './open-orders'

/** The chart carries at most this many orders, the newest. */
export const MAX_MARKS = 60
/** A full contract, in the exchange's price ticks. */
const FULL_TICKS = 1000
const TICKS_PER_PERCENT = 10

/**
 * The account's filled orders as marks for the chart: when each was placed,
 * which outcome it bought and what it paid. An order that filled nothing,
 * or that an older server sent without a clock time, has no place on a
 * time axis and is left out. `no` is shown as buying the second outcome at
 * its own price, the way the trade history shows it.
 */
export function toTradeMarks(
  trades: readonly RustUserTradeRow[],
  outcomes: readonly [MarketOutcome, MarketOutcome],
  cards: readonly StrategyCardName[]
): TradeMark[] {
  const marks: TradeMark[] = []
  for (const trade of trades) {
    if (trade.at_ms === undefined || trade.avg_price === undefined || trade.filled_qty === 0) {
      continue
    }
    const first = trade.side === 'yes'
    const paid = first ? trade.avg_price : FULL_TICKS - trade.avg_price
    const who = trade.strategy_name === undefined ? 'You' : cardNameOf(trade.strategy_name, cards)
    marks.push({
      id: `${trade.strategy_id ?? 'main'}:${trade.order_id}`,
      label: `${who} bought ${trade.filled_qty} ${outcomeLabel(outcomes, first)} at ${formatExchangeTicks(paid)}`,
      percent: paid / TICKS_PER_PERCENT,
      side: first ? 'first' : 'second',
      time: trade.at_ms,
    })
  }
  return marks.toSorted((a, b) => a.time - b.time).slice(-MAX_MARKS)
}
