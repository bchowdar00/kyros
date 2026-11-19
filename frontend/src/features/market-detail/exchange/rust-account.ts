import {
  exchangeMarketKey,
  type RustAccountsFrame,
  type RustPositionFrame,
  tenthsToMicros,
} from '@kyros/domain'

import type { AccountState } from './account'

/**
 * A positions snapshot is absolute: balance, lock and the signed position for this series.
 * Positive is YES lots, negative is NO lots. The exchange charges no fees, so every market
 * the account can see carries a zero rate and the buy quote never waits on one.
 */
export function applyRustPosition(previous: AccountState, frame: RustPositionFrame): AccountState {
  const positions =
    frame.market_id === null
      ? previous.positions
      : [
          ...previous.positions.filter(
            position => position.market_id !== exchangeMarketKey(String(frame.market_id))
          ),
          {
            market_id: exchangeMarketKey(String(frame.market_id)),
            yes: String(Math.max(frame.signed_position, 0)),
            no: String(Math.max(-frame.signed_position, 0)),
          },
        ]
  const fees =
    frame.market_id === null
      ? previous.fees
      : { ...previous.fees, [exchangeMarketKey(String(frame.market_id))]: 0 }

  return {
    ...previous,
    status: 'live',
    balanceSeq: String(frame.as_of_unix_ms),
    available: tenthsToMicros(frame.balance_tenths),
    locked: tenthsToMicros(frame.locked_tenths),
    positions,
    fees,
  }
}

/**
 * An accounts snapshot is absolute too: the exchange rebuilds the main row, the strategy
 * rows, every resting order and the order log on each broadcast and pushes only on change,
 * so the lists replace rather than merge. The frame spans every series; this page keeps
 * the main account plus what belongs to `series`: the strategies homed there, and the
 * orders and log rows placed there. Settlement cancels resting orders server-side, which
 * arrives here as an empty `open_orders`; the log keeps its settled rows.
 */
export function applyRustAccounts(
  previous: AccountState,
  frame: RustAccountsFrame,
  series: string
): AccountState {
  return {
    ...previous,
    accounts: [frame.main, ...frame.strategies.filter(row => row.home_series === series)],
    openOrders: frame.open_orders.filter(order => order.series === series),
    trades: frame.trades.filter(trade => trade.series === series),
  }
}
