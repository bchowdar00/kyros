import { render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'

import { MINUTE } from '../charts/lib/time'
import { ProbabilityMarketCard } from '../charts/probability-market-card'

/** jsdom lays nothing out; the chart only draws once its container reports a size. */
function giveChartsASize() {
  const original = HTMLElement.prototype.getBoundingClientRect
  vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(function (
    this: HTMLElement
  ) {
    return this.tagName === 'FIGURE' ? new DOMRect(0, 0, 1110, 292) : original.call(this)
  })
}

const start = Date.UTC(2026, 10, 20, 10, 0)
const range = { end: start + 10 * MINUTE, start }
const points = [
  { time: start, value: 55 },
  { time: start + 4 * MINUTE, value: 61 },
]

function card(extra: Partial<Parameters<typeof ProbabilityMarketCard>[0]> = {}) {
  return render(
    <ProbabilityMarketCard
      outcomes={[
        { color: 'var(--positive)', id: 'home', label: 'BOS', points },
        { color: 'var(--negative)', id: 'away', label: 'NYK' },
      ]}
      range={range}
      timeZone="UTC"
      {...extra}
    />
  )
}

describe('ProbabilityMarketCard', () => {
  afterEach(() => {
    vi.restoreAllMocks()
  })

  it('marks each order at the price paid for the side it bought', () => {
    giveChartsASize()
    card({
      tradeMarks: [
        { id: '1', label: 'Bought 5 BOS at 58¢', percent: 58, side: 'first', time: start + MINUTE },
        {
          id: '2',
          label: 'Bought 5 NYK at 40¢',
          percent: 40,
          side: 'second',
          time: start + 3 * MINUTE,
        },
      ],
    })

    expect(screen.getByRole('img', { name: 'Bought 5 BOS at 58¢' })).toHaveAttribute(
      'data-trade-mark',
      'first'
    )
    expect(screen.getByRole('img', { name: 'Bought 5 NYK at 40¢' })).toHaveAttribute(
      'data-trade-mark',
      'second'
    )
  })

  it('draws another venue as a quiet line with its name at the end', () => {
    giveChartsASize()
    const { container } = card({
      venueLines: [
        {
          id: 'kalshi',
          label: 'Kalshi',
          points: [
            { time: start, value: 53 },
            { time: start + 4 * MINUTE, value: 60 },
          ],
        },
      ],
    })

    const line = container.querySelector('[data-venue-line="kalshi"]')
    expect(line).not.toBeNull()
    expect(line?.querySelector('polyline')).not.toBeNull()
    expect(line?.querySelector('text')?.textContent).toBe('Kalshi 60%')
  })

  it("writes a venue's name to the right of a line that ends near the left edge", () => {
    giveChartsASize()
    const { container } = card({
      venueLines: [
        {
          id: 'kalshi',
          label: 'Kalshi',
          points: [
            { time: start, value: 53 },
            { time: start + 2000, value: 54 },
          ],
        },
      ],
    })

    const label = container.querySelector('[data-venue-line="kalshi"] text')
    expect(label?.getAttribute('text-anchor')).toBe('start')
  })

  it('draws no venue line from fewer than two readings', () => {
    giveChartsASize()
    const { container } = card({
      venueLines: [{ id: 'kalshi', label: 'Kalshi', points: [{ time: start, value: 53 }] }],
    })

    expect(container.querySelector('[data-venue-line]')).toBeNull()
  })
})
