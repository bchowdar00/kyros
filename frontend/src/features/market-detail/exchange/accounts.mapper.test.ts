import type { RustAccountRow } from '@kyros/domain'
import { cents } from '@kyros/money'
import { describe, expect, it } from 'vitest'

import type { MarketOutcome, MyStrategy } from '../types'
import { accountRows, rankedRows, strategyRank } from './accounts.mapper'

const me = `0x${'aa'.repeat(20)}`
const child = `0x${'cc'.repeat(20)}`
const outcomes: readonly [MarketOutcome, MarketOutcome] = [
  { label: 'UP', side: 'home' },
  { label: 'DOWN', side: 'away' },
]

const position = (series: string, signed: number, cost: number, roundPnl = 0) => ({
  series,
  market_id: 7,
  side: signed > 0 ? ('yes' as const) : ('no' as const),
  size_shares: Math.abs(signed),
  signed_position: signed,
  avg_entry_yes_tenths: 480,
  cost_basis_tenths: cost,
  mark_yes_tenths: 470,
  mark_value_tenths: 1_880,
  unrealized_pnl_tenths: 0,
  market_pnl_tenths: roundPnl,
})

const main: RustAccountRow = {
  id: me,
  kind: 'main',
  name: 'jet',
  status: null,
  home_series: null,
  trades_this_market: 2,
  // Round PnL lives on the series row; the lifetime figure is deliberately different so
  // the test would catch the mapper reading the wrong one.
  series_positions: [position('btc-5m', 40, 27_500, 2_000), position('btc-15m', -3, 1_500, 900)],
  balance_tenths: 500_000,
  locked_tenths: 0,
  market_pnl_total_tenths: 0,
  lifetime_pnl_tenths: 2_000,
}

const strategy: RustAccountRow = {
  id: child,
  kind: 'strategy',
  name: 'Momentum',
  status: 'paused',
  home_series: 'btc-5m',
  trades_this_market: 0,
  // The other series' row is deliberately non-zero: it must never leak into this page.
  series_positions: [position('btc-5m', -4, -2_080, -9_100), position('btc-15m', 7, 3_500, 5_000)],
  balance_tenths: 200_000,
  locked_tenths: 0,
  market_pnl_total_tenths: -100,
  lifetime_pnl_tenths: -9_100,
}

describe('accountRows', () => {
  it('shows a strategy under the title of the card it was run from, and leaves the main account alone', () => {
    const cards = [{ title: 'Flash Move', primaryName: 'Momentum', inverseName: 'jet' }]
    const rows = accountRows([main, strategy], 'btc-5m', outcomes, cards)
    expect(rows.map(row => row.name)).toEqual(['jet', 'Flash Move'])
  })

  it('keeps the exchange order, main first, and shows this series for the main account', () => {
    const rows = accountRows([main, strategy], 'btc-5m', outcomes)
    expect(rows.map(row => row.name)).toEqual(['jet', 'Momentum'])
    expect(rows[0]).toMatchObject({
      id: me,
      status: 'active',
      totalPnlCents: 200,
      trades: 2,
      position: { label: '40 UP', shares: 40, side: 'home', valueCents: 188 },
    })
  })

  it('shows a strategy with paused status and a short as DOWN, from this series only', () => {
    const [, row] = accountRows([main, strategy], 'btc-5m', outcomes)
    expect(row).toMatchObject({
      status: 'paused',
      totalPnlCents: -910,
      position: { label: '4 DOWN', shares: 4, side: 'away', valueCents: 188 },
    })
  })

  it('shows no position and zero P&L for an account that never traded this series', () => {
    const [row] = accountRows([{ ...main, series_positions: [] }], 'btc-5m', outcomes)
    expect(row?.position).toBeUndefined()
    expect(row?.totalPnlCents).toBe(0)
  })

  it('rounds tenths of a cent to whole cents, never a float', () => {
    const odd = { ...main, series_positions: [position('btc-5m', 40, 27_500, -407)] }
    const [row] = accountRows([odd], 'btc-5m', outcomes)
    expect(row?.totalPnlCents).toBe(-41)
    expect(Number.isInteger(row?.totalPnlCents)).toBe(true)
  })

  it('leaves the position off a flat account', () => {
    const flat = { ...main, series_positions: [position('btc-5m', 0, 0)] }
    expect(accountRows([flat], 'btc-5m', outcomes)[0]?.position).toBeUndefined()
  })

  it('falls back to what the position cost while nothing can mark it', () => {
    const unmarked = {
      ...main,
      series_positions: [{ ...position('btc-5m', 40, 27_500), mark_value_tenths: null }],
    }
    expect(accountRows([unmarked], 'btc-5m', outcomes)[0]?.position?.valueCents).toBe(2_750)
  })
})

const rankedRow = (id: string, shares?: number): MyStrategy => ({
  id,
  name: id,
  status: 'active',
  totalPnlCents: cents(0),
  trades: 0,
  ...(shares === undefined
    ? {}
    : { position: { label: `${shares} UP`, shares, side: 'home', valueCents: cents(0) } }),
})

describe('strategy ranking', () => {
  const rows = [
    rankedRow('main', 1),
    rankedRow('a', 3),
    rankedRow('b', 9),
    rankedRow('c'),
    rankedRow('d', 9),
  ]

  it('ranks strategies by shares, largest first, ties in exchange order, main excluded', () => {
    expect(strategyRank(rows)).toEqual(['b', 'd', 'a', 'c'])
  })

  it('keeps main first, follows the last rank, and sizes newcomers in after it', () => {
    const stale = ['a', 'b']
    expect(rankedRows(rows, stale).map(item => item.id)).toEqual(['main', 'a', 'b', 'd', 'c'])
    expect(rankedRows(rows, []).map(item => item.id)).toEqual(['main', 'b', 'd', 'a', 'c'])
    expect(rankedRows([], stale)).toEqual([])
  })
})
