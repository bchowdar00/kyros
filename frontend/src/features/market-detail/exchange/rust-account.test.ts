import { exchangeMarketKey, type RustAccountsFrame } from '@kyros/domain'
import { describe, expect, it } from 'vitest'

import { initialAccount } from './account'
import { applyRustAccounts, applyRustPosition } from './rust-account'

const me = `0x${'aa'.repeat(20)}`
const child = `0x${'cc'.repeat(20)}`

const snapshot = {
  type: 'position_snapshot' as const,
  as_of_unix_ms: 5,
  series: 'btc-5m',
  market_id: 7,
  address: me,
  side: 'no' as const,
  size_shares: 4,
  signed_position: -4,
  avg_entry_yes_tenths: 480,
  cost_basis_tenths: -2_080,
  mark_yes_tenths: 470,
  unrealized_pnl_tenths: 40,
  realized_pnl_this_market_tenths: 0,
  balance_tenths: 997_920,
  locked_tenths: 2_080,
  series_positions: [],
}

const row = (id: string, kind: 'main' | 'strategy'): RustAccountsFrame['main'] => ({
  id,
  kind,
  name: kind === 'main' ? 'jet' : 'Momentum',
  status: kind === 'main' ? null : 'active',
  home_series: kind === 'main' ? null : 'btc-5m',
  trades_this_market: 0,
  series_positions: [],
  balance_tenths: 500_000,
  locked_tenths: 0,
  market_pnl_total_tenths: 0,
  lifetime_pnl_tenths: 0,
})

const accounts: RustAccountsFrame = {
  type: 'accounts_snapshot',
  as_of_unix_ms: 6,
  address: me,
  main: row(me, 'main'),
  strategies: [row(child, 'strategy')],
  open_orders: [
    {
      series: 'btc-5m',
      order_id: 9,
      market_id: 7,
      account: child,
      account_kind: 'strategy',
      account_name: 'Momentum',
      side: 'bid',
      price: 450,
      qty_open: 2,
      placed_ts: 3,
    },
  ],
  trades: [
    {
      series: 'btc-5m',
      order_id: 8,
      ts: 2,
      side: 'yes',
      qty_requested: 1,
      filled_qty: 1,
      avg_price: 500,
      source: 'ui',
      order_type: 'market',
      status: 'filled',
      market_id: 7,
      realized_pnl_tenths: 0,
      settled: false,
    },
  ],
}

describe('rust account adapter', () => {
  it('converts tenths to micros, splits the signed position and zeroes the fee', () => {
    const account = applyRustPosition(initialAccount, snapshot)
    expect(account.status).toBe('live')
    expect(account.available).toBe('997920000')
    expect(account.locked).toBe('2080000')
    expect(account.positions).toEqual([{ market_id: exchangeMarketKey('7'), yes: '0', no: '4' }])
    expect(account.fees[exchangeMarketKey('7')]).toBe(0)
    expect(applyRustPosition(account, { ...snapshot, signed_position: 3 }).positions[0]?.yes).toBe(
      '3'
    )
  })

  it('replaces the account list, open orders and trade log from each accounts frame', () => {
    const account = applyRustAccounts(initialAccount, accounts, 'btc-5m')
    expect(account.accounts.map(item => item.id)).toEqual([me, child])
    expect(account.openOrders).toHaveLength(1)
    expect(account.openOrders[0]?.account).toBe(child)
    expect(account.trades).toHaveLength(1)

    // A settlement frame carries no resting orders but keeps the (now settled) log.
    const settled = applyRustAccounts(
      account,
      {
        ...accounts,
        open_orders: [],
        trades: accounts.trades.map(trade => ({ ...trade, settled: true })),
      },
      'btc-5m'
    )
    expect(settled.openOrders).toEqual([])
    expect(settled.trades[0]?.settled).toBe(true)
  })

  it('keeps only this series: its strategies, its orders and its log rows', () => {
    const elsewhere = `0x${'dd'.repeat(20)}`
    const [order] = accounts.open_orders
    const [trade] = accounts.trades
    if (order === undefined || trade === undefined) throw new Error('fixture')
    const twoSeries: RustAccountsFrame = {
      ...accounts,
      strategies: [
        row(child, 'strategy'),
        { ...row(elsewhere, 'strategy'), name: 'Slow', home_series: 'btc-15m' },
      ],
      open_orders: [
        order,
        { ...order, order_id: 10, series: 'btc-15m', account: elsewhere, account_name: 'Slow' },
      ],
      trades: [trade, { ...trade, order_id: 11, series: 'btc-15m' }],
    }

    const fast = applyRustAccounts(initialAccount, twoSeries, 'btc-5m')
    expect(fast.accounts.map(item => item.id)).toEqual([me, child])
    expect(fast.openOrders.map(item => item.order_id)).toEqual([9])
    expect(fast.trades.map(item => item.order_id)).toEqual([8])

    // The main account is on every page; the rest follows the series.
    const slow = applyRustAccounts(initialAccount, twoSeries, 'btc-15m')
    expect(slow.accounts.map(item => item.id)).toEqual([me, elsewhere])
    expect(slow.openOrders.map(item => item.order_id)).toEqual([10])
    expect(slow.trades.map(item => item.order_id)).toEqual([11])
  })

  it('leaves the accounts lists alone when only the position frame changes', () => {
    const account = applyRustPosition(
      applyRustAccounts(initialAccount, accounts, 'btc-5m'),
      snapshot
    )
    expect(account.openOrders).toHaveLength(1)
    expect(account.trades).toHaveLength(1)
  })
})
