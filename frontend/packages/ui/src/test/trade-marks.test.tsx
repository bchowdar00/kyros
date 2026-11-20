import { render, screen } from '@testing-library/react'
import { describe, expect, it } from 'vitest'

import { MINUTE } from '../charts/lib/time'
import { MarketChart } from '../charts/market-chart'
import { type TradeMark, TradeMarks, valueOnLine } from '../charts/trade-marks'

const start = Date.UTC(2026, 10, 20, 10, 0)
const range = { end: start + 5 * MINUTE, start }
const points = [
  { time: start, value: 40 },
  { time: start + 2 * MINUTE, value: 60 },
  { time: start + 4 * MINUTE, value: 50 },
]
const series = [{ color: 'var(--positive)', id: 'yes', label: 'Up', points }]

function mark(id: string, minute: number, side: TradeMark['side'], percent = 55): TradeMark {
  return { id, label: `Order ${id}`, percent, side, time: start + minute * MINUTE }
}

function chart(marks: readonly TradeMark[]) {
  return render(
    <MarketChart height={292} kind="probability" range={range} series={series} width={1110}>
      <TradeMarks marks={marks} valueOf={each => each.percent} />
    </MarketChart>
  )
}

describe('TradeMarks', () => {
  it('draws one mark per order where it was placed, pointing up for the first side and down for the second', () => {
    const { container } = chart([mark('a', 1, 'first'), mark('b', 3, 'second')])

    const drawn = [...container.querySelectorAll('[data-trade-mark]')]
    expect(drawn.map(each => each.getAttribute('data-trade-mark'))).toEqual(['first', 'second'])
    expect(screen.getByRole('img', { name: 'Order a' })).toBeInTheDocument()
    expect(screen.getByRole('img', { name: 'Order b' })).toBeInTheDocument()
    // Earlier orders sit to the left of later ones.
    const [first, second] = drawn.map(each => Number(each.getAttribute('data-x')))
    expect(first).toBeLessThan(second ?? 0)
  })

  it('draws orders placed almost together on the same side as one mark, and says how many', () => {
    const { container } = chart([
      mark('a', 1, 'first'),
      mark('b', 1.004, 'first'),
      mark('c', 1.008, 'first'),
      mark('d', 1.004, 'second'),
    ])

    expect(container.querySelectorAll('[data-trade-mark="first"]')).toHaveLength(1)
    expect(container.querySelectorAll('[data-trade-mark="second"]')).toHaveLength(1)
    expect(screen.getByRole('img', { name: 'Order a, and 2 more orders' })).toBeInTheDocument()
  })

  it('leaves out an order outside the window, and one with nowhere to sit', () => {
    const { container } = chart([
      mark('before', -3, 'first'),
      { id: 'unplaced', label: 'Order unplaced', side: 'first', time: start + MINUTE },
    ])

    expect(container.querySelector('[data-trade-mark]')).toBeNull()
  })
})

describe('a mark on a moving chart', () => {
  it('stays the same element when it moves across the plot, so pointing at it keeps working', () => {
    const marks = [mark('a', 2, 'first')]
    const view = (width: number) => (
      <MarketChart height={292} kind="probability" range={range} series={series} width={width}>
        <TradeMarks marks={marks} valueOf={each => each.percent} />
      </MarketChart>
    )
    const { container, rerender } = render(view(1110))
    const before = container.querySelector('[data-trade-mark]')
    const x = Number(before?.getAttribute('data-x'))

    rerender(view(900))

    const after = container.querySelector('[data-trade-mark]')
    expect(Number(after?.getAttribute('data-x'))).not.toBe(x)
    expect(after).toBe(before)
  })
})

describe('valueOnLine', () => {
  it('reads the line at a time between two samples, and at its ends', () => {
    expect(valueOnLine(points, start + MINUTE)).toBe(50)
    expect(valueOnLine(points, start + 2 * MINUTE)).toBe(60)
    expect(valueOnLine(points, start + 3 * MINUTE)).toBe(55)
  })

  it('has no value before the line starts, after it ends, or with no line', () => {
    expect(valueOnLine(points, start - 1)).toBeUndefined()
    expect(valueOnLine(points, start + 4 * MINUTE + 1)).toBeUndefined()
    expect(valueOnLine([], start)).toBeUndefined()
  })
})
