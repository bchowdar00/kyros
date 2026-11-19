import { createMoneyFormatter } from '@kyros/money'

import { Ladder, type LadderLevel } from '../widgets/ladder'

export interface OrderBookProps {
  series: string
  /** Best ask first, prices in cents. */
  asks: readonly LadderLevel[]
  /** Best bid first, prices in cents. */
  bids: readonly LadderLevel[]
  /** Levels shown a side; deeper books scroll inside the ladder. */
  rows?: number
}

const money = createMoneyFormatter('en-US')
const LEVELS = 7

function sizeWithin(levels: readonly LadderLevel[], rows: number) {
  return levels.slice(0, rows).reduce((total, level) => total + level.size, 0)
}

/**
 * The canonical YES book, seven levels a side by default. Imbalance is bid depth against
 * ask depth over the shown levels, from −1 (all asks) to +1 (all bids).
 */
export function OrderBook({ series, asks, bids, rows = LEVELS }: OrderBookProps) {
  const bidDepth = sizeWithin(bids, rows)
  const askDepth = sizeWithin(asks, rows)
  const imbalance = (bidDepth - askDepth) / Math.max(1, bidDepth + askDepth)
  const bestAsk = asks[0]?.price
  const bestBid = bids[0]?.price
  const spread = bestAsk !== undefined && bestBid !== undefined ? bestAsk - bestBid : undefined

  return (
    <Ladder
      label={`${series} order book`}
      title="Order book"
      meta={
        <span className="font-mono">
          Imbalance {imbalance >= 0 ? '+' : '−'}
          {Math.abs(imbalance).toFixed(2)}
        </span>
      }
      asks={asks}
      bids={bids}
      rows={rows}
      formatPrice={price => `${price}¢`}
      formatSize={money.count}
      {...(spread === undefined ? {} : { midLabel: `Spread ${spread}¢` })}
    />
  )
}
