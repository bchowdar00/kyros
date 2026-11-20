import type { RustUserTradeRow } from '@kyros/domain'
import { describe, expect, it } from 'vitest'

import type { MarketOutcome } from '../types'
import { MAX_MARKS, toTradeMarks } from './trade-marks'

const outcomes = [
  { label: 'UP', side: 'home' },
  { label: 'DOWN', side: 'away' },
] as unknown as readonly [MarketOutcome, MarketOutcome]

const AT = Date.UTC(2026, 10, 20, 11, 0)

const row = (id: number, extra: Partial<RustUserTradeRow> = {}): RustUserTradeRow => ({
  series: 'btc-5m',
  order_id: id,
  ts: id,
  at_ms: AT + id * 1000,
  side: 'yes',
  qty_requested: 3,
  filled_qty: 3,
  avg_price: 590,
  source: 'ui',
  order_type: 'market',
  status: 'filled',
  market_id: 7,
  realized_pnl_tenths: 0,
  settled: false,
  ...extra,
})

describe('toTradeMarks', () => {
  it('marks a filled order at its time and the price paid, and says who bought what', () => {
    expect(toTradeMarks([row(1)], outcomes, [])).toEqual([
      {
        id: 'main:1',
        label: 'You bought 3 UP at 59.0¢',
        percent: 59,
        side: 'first',
        time: AT + 1000,
      },
    ])
  })

  it('shows the other side as the second outcome at its own price, under the strategy that placed it', () => {
    const [mark] = toTradeMarks(
      [row(2, { side: 'no', avg_price: 600, strategy_id: '0xabc', strategy_name: 'Momentum' })],
      outcomes,
      [{ title: 'Flash Move', primaryName: 'Momentum', inverseName: 'jet' }]
    )

    expect(mark).toEqual({
      id: '0xabc:2',
      label: 'Flash Move bought 3 DOWN at 40.0¢',
      percent: 40,
      side: 'second',
      time: AT + 2000,
    })
  })

  it('leaves out an order that filled nothing, one with no price and one with no clock time', () => {
    const marks = toTradeMarks(
      [
        row(1, { filled_qty: 0, status: 'killed' }),
        row(2, { avg_price: undefined }),
        row(3, { at_ms: undefined }),
      ],
      outcomes,
      []
    )

    expect(marks).toEqual([])
  })

  it('keeps only the newest orders', () => {
    const many = Array.from({ length: MAX_MARKS + 5 }, (_, index) => row(index + 1))

    const marks = toTradeMarks(many, outcomes, [])

    expect(marks).toHaveLength(MAX_MARKS)
    expect(marks.some(mark => mark.id === 'main:1')).toBe(false)
    expect(marks.some(mark => mark.id === `main:${MAX_MARKS + 5}`)).toBe(true)
  })
})
