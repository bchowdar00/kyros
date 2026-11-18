'use client'

import { useState } from 'react'
import { formatExchangeTicks } from '@tiny/money'
import { Button } from '@tiny/ui/components/button'

import type { MarketOutcome } from '../types'
import type { AccountState } from './account'
import type { RustExchangeStore } from './rust-exchange-store'

export const ACTIVITY_PAGE_SIZE = 5

/** `home` is YES on the canonical book; a bid buys it, an ask sells it (buys the other side). */
export function outcomeLabel(
  outcomes: readonly [MarketOutcome, MarketOutcome],
  yes: boolean
): string {
  const side = yes ? 'home' : 'away'
  return (outcomes.find(outcome => outcome.side === side) ?? outcomes[0]).label
}

export function ActivityPager({
  page,
  pages,
  label,
  onPage,
}: {
  page: number
  pages: number
  label: string
  onPage: (page: number) => void
}) {
  if (pages <= 1) return null
  return (
    <nav className="flex items-center gap-lg" aria-label={label}>
      <Button disabled={page === 0} onClick={() => onPage(page - 1)}>
        Previous
      </Button>
      <span>
        {page + 1} / {pages}
      </span>
      <Button disabled={page === pages - 1} onClick={() => onPage(page + 1)}>
        Next
      </Button>
    </nav>
  )
}

/**
 * Every resting order the user and their strategies hold, straight from the accounts
 * feed: the main account's rows first, then each strategy's. A cancel goes out as the
 * owning account, and the row disappears when the exchange confirms on the next frame.
 */
export function ExchangeOpenOrders({
  account,
  outcomes,
  store,
}: {
  account: AccountState
  outcomes: readonly [MarketOutcome, MarketOutcome]
  store: Pick<RustExchangeStore, 'cancel'>
}) {
  const [page, setPage] = useState(0)
  const orders = account.openOrders
  const pages = Math.max(1, Math.ceil(orders.length / ACTIVITY_PAGE_SIZE))
  const visiblePage = Math.min(page, pages - 1)
  const start = visiblePage * ACTIVITY_PAGE_SIZE
  return (
    <section
      className="flex h-full flex-col gap-lg rounded-md border border-border-subtle p-xl"
      aria-label="Open orders"
    >
      {orders.length === 0 && (
        <p className="text-label text-muted-foreground">
          {account.status === 'live' ? 'No open orders.' : 'Waiting for account snapshot.'}
        </p>
      )}
      {orders.slice(start, start + ACTIVITY_PAGE_SIZE).map(order => (
        <div
          className="flex items-center justify-between gap-lg text-label"
          key={`${order.account}:${order.order_id}`}
        >
          <span className="flex min-w-0 flex-col">
            <span className="truncate font-medium text-foreground">{order.account_name}</span>
            <span className="font-mono text-muted-foreground">
              Buy {order.qty_open} {outcomeLabel(outcomes, order.side === 'bid')} at{' '}
              {formatExchangeTicks(order.side === 'bid' ? order.price : 1000 - order.price)} ·{' '}
              {order.series}
            </span>
          </span>
          <Button
            className="min-h-touch"
            disabled={account.status !== 'live'}
            onClick={() => void store.cancel(order)}
            aria-label={`Cancel ${order.account_name} order ${order.order_id}`}
          >
            Cancel
          </Button>
        </div>
      ))}
      <ActivityPager label="Open order pages" onPage={setPage} page={visiblePage} pages={pages} />
    </section>
  )
}
