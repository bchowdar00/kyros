import { render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import type { RustOpenOrderRow, RustUserTradeRow } from '@tiny/domain'
import { expectNoA11yViolations } from '@tiny/testing/a11y'
import { describe, expect, it, vi } from 'vitest'

import type { MarketOutcome } from '../types'
import { type AccountState, initialAccount } from './account'
import { ExchangeOpenOrders } from './open-orders'
import { ExchangeTradeHistory } from './trade-history'

const me = `0x${'aa'.repeat(20)}`
const child = `0x${'cc'.repeat(20)}`
const outcomes: readonly [MarketOutcome, MarketOutcome] = [
  { label: 'UP', side: 'home' },
  { label: 'DOWN', side: 'away' },
]

const order = (
  account: string,
  name: string,
  id: number,
  side: 'bid' | 'ask'
): RustOpenOrderRow => ({
  series: 'btc-2m',
  order_id: id,
  market_id: 7,
  account,
  account_kind: account === me ? 'main' : 'strategy',
  account_name: name,
  side,
  price: 310,
  qty_open: 2,
  placed_ts: id,
})

const trade = (id: number, strategy?: { id: string; name: string }): RustUserTradeRow => ({
  series: 'btc-2m',
  order_id: id,
  ts: id,
  side: strategy === undefined ? 'yes' : 'no',
  qty_requested: 3,
  filled_qty: 3,
  avg_price: 400,
  source: strategy === undefined ? 'ui' : 'strategy',
  order_type: 'market',
  status: 'filled',
  market_id: 7,
  ...(strategy === undefined ? {} : { strategy_id: strategy.id, strategy_name: strategy.name }),
  realized_pnl_tenths: strategy === undefined ? 0 : 1_250,
  settled: strategy !== undefined,
})

const live: AccountState = {
  ...initialAccount,
  status: 'live',
  openOrders: [order(me, 'jet', 1, 'bid'), order(child, 'Momentum', 2, 'ask')],
  trades: [trade(2, { id: child, name: 'Momentum' }), trade(1)],
}

describe('ExchangeOpenOrders', () => {
  it('lists the main account and strategy orders and cancels as the owning account', async () => {
    const user = userEvent.setup()
    const cancel = vi.fn(async () => undefined)
    const { container } = render(
      <ExchangeOpenOrders account={live} outcomes={outcomes} store={{ cancel }} />
    )

    const region = screen.getByRole('region', { name: 'Open orders' })
    expect(region).toHaveTextContent('jet')
    expect(region).toHaveTextContent('Buy 2 UP at 31.0¢')
    expect(region).toHaveTextContent('Momentum')
    expect(region).toHaveTextContent('Buy 2 DOWN at 69.0¢')

    await user.click(screen.getByRole('button', { name: 'Cancel Momentum order 2' }))
    expect(cancel).toHaveBeenCalledWith(expect.objectContaining({ account: child, order_id: 2 }))
    await expectNoA11yViolations(container)
  })

  it('explains an empty list by connection state', () => {
    render(
      <ExchangeOpenOrders
        account={initialAccount}
        outcomes={outcomes}
        store={{ cancel: vi.fn() }}
      />
    )
    expect(screen.getByText('Waiting for account snapshot.')).toBeInTheDocument()
  })
})

describe('ExchangeTradeHistory', () => {
  it('shows main and strategy rows newest first with settled state and realized P&L', async () => {
    const { container } = render(<ExchangeTradeHistory account={live} outcomes={outcomes} />)

    const table = screen.getByRole('table', { name: 'Orders placed by you and your strategies' })
    const rows = within(table).getAllByRole('row').slice(1)
    expect(rows).toHaveLength(2)
    expect(rows[0]).toHaveTextContent('Momentum')
    expect(rows[0]).toHaveTextContent('DOWN')
    expect(rows[0]).toHaveTextContent('60.0¢')
    expect(rows[0]).toHaveTextContent('Filled · Settled')
    expect(rows[0]).toHaveTextContent('+$1.25')
    expect(rows[1]).toHaveTextContent('Main')
    expect(rows[1]).toHaveTextContent('UP')
    expect(rows[1]).toHaveTextContent('40.0¢')
    expect(rows[1]).toHaveTextContent('3/3')
    await expectNoA11yViolations(container)
  })
})
