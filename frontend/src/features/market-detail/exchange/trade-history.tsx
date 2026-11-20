'use client'

import { useState } from 'react'
import type { RustUserTradeRow } from '@kyros/domain'
import { cents, createMoneyFormatter, formatExchangeTicks } from '@kyros/money'

import { cardNameOf, type StrategyCardName } from '../strategy-card-names'
import type { MarketOutcome } from '../types'
import type { AccountState } from './account'
import { ACTIVITY_PAGE_SIZE, ActivityPager, outcomeLabel } from './open-orders'

const money = createMoneyFormatter('en-US')
const TENTHS_PER_CENT = 10

const statusLabels: Record<RustUserTradeRow['status'], string> = {
  filled: 'Filled',
  partially_filled: 'Partial',
  rested: 'Open',
  killed: 'Killed',
  cancelled: 'Cancelled',
}

/** The YES price of the fill (or the limit while nothing filled), shown from the buyer's side. */
function priceOf(trade: RustUserTradeRow): string {
  const yesPrice = trade.avg_price ?? trade.limit_price
  if (yesPrice === undefined) return 'Market'
  return formatExchangeTicks(trade.side === 'yes' ? yesPrice : 1000 - yesPrice)
}

function pnlOf(trade: RustUserTradeRow): string {
  if (trade.realized_pnl_tenths === 0) return '—'
  return money.usd(cents(Math.round(trade.realized_pnl_tenths / TENTHS_PER_CENT)), {
    signDisplay: 'always',
  })
}

/**
 * The order log across the main account and every strategy, newest first, from the
 * accounts feed. Rows from resolved rounds stay and are marked settled, so the tab is
 * not wiped when the market flips.
 */
export function ExchangeTradeHistory({
  account,
  cards = [],
  outcomes,
}: {
  account: AccountState
  /** The series' public cards: a strategy run from one is named by the card's title. */
  cards?: readonly StrategyCardName[] | undefined
  outcomes: readonly [MarketOutcome, MarketOutcome]
}) {
  const [page, setPage] = useState(0)
  const trades = account.trades
  const pages = Math.max(1, Math.ceil(trades.length / ACTIVITY_PAGE_SIZE))
  const visiblePage = Math.min(page, pages - 1)
  const start = visiblePage * ACTIVITY_PAGE_SIZE
  return (
    <section
      className="flex h-full flex-col gap-lg rounded-md border border-border-subtle p-xl"
      aria-label="Trade history"
    >
      {trades.length === 0 ? (
        <p className="text-label text-muted-foreground">
          {account.status === 'live' ? 'No trades yet.' : 'Waiting for account snapshot.'}
        </p>
      ) : (
        <table className="w-full text-left text-label">
          <caption className="sr-only">Orders placed by you and your strategies</caption>
          <thead>
            <tr className="text-label font-semibold text-muted-foreground">
              <th scope="col">Account</th>
              <th scope="col">Side</th>
              <th scope="col">Filled</th>
              <th scope="col">Price</th>
              <th scope="col">Status</th>
              <th className="text-right" scope="col">
                P&amp;L
              </th>
            </tr>
          </thead>
          <tbody>
            {trades.slice(start, start + ACTIVITY_PAGE_SIZE).map(trade => (
              <tr key={`${trade.strategy_id ?? 'main'}:${trade.order_id}`}>
                <td className="truncate py-sm">
                  {trade.strategy_name === undefined
                    ? 'Main'
                    : cardNameOf(trade.strategy_name, cards)}
                </td>
                <td>{outcomeLabel(outcomes, trade.side === 'yes')}</td>
                <td className="font-mono">
                  {trade.filled_qty}/{trade.qty_requested}
                </td>
                <td className="font-mono">{priceOf(trade)}</td>
                <td>
                  {statusLabels[trade.status]}
                  {trade.settled ? ' · Settled' : ''}
                </td>
                <td className="text-right font-mono">{pnlOf(trade)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      <ActivityPager
        label="Trade history pages"
        onPage={setPage}
        page={visiblePage}
        pages={pages}
      />
    </section>
  )
}
