import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { describe, expect, it, vi } from 'vitest'

import { SignalCard } from './signal-card'
import type { MarketSignal } from './types'

const signal = {
  id: 'btc.polymarket.prediction_quote.v1:affirmative_bid',
  context: 'Polymarket venue price',
  label: 'Polymarket up bid',
  value: '55¢',
}

const reading: MarketSignal = { id: 'spot.price', label: 'Coinbase price', value: '61,969.57' }
const at = (second: number, value: number) => ({ time: 1_795_170_000 + second, value })
const rising = [at(0, 61_900), at(1, 61_940), at(2, 61_969.57)]
const falling = [at(0, 61_990), at(1, 61_980), at(2, 61_969.57)]

function card(): HTMLElement {
  return screen.getByRole('article')
}

describe('SignalCard', () => {
  it('is a static reading without an opener', async () => {
    const { container } = render(<SignalCard signal={signal} />)
    expect(screen.queryByRole('button')).not.toBeInTheDocument()
    expect(screen.getByText('Polymarket up bid')).toHaveAttribute('title', 'Polymarket venue price')
    expect(screen.getByText('55¢')).toBeInTheDocument()
    await expectNoA11yViolations(container)
  })

  it('opens its details when given an opener', async () => {
    const user = userEvent.setup()
    const onOpen = vi.fn()
    const { container } = render(<SignalCard onOpen={onOpen} signal={signal} />)
    const opener = screen.getByRole('button', { name: 'Polymarket up bid, 55¢. Open details' })
    await user.click(opener)
    expect(onOpen).toHaveBeenCalledWith(signal.id, opener)
    await expectNoA11yViolations(container)
  })

  it('keeps its light and its trace when it opens details', () => {
    render(<SignalCard onOpen={vi.fn()} signal={{ ...reading, trace: rising }} />)

    const opener = screen.getByRole('button')
    expect(opener).toHaveAttribute('data-trend', 'up')
    expect(opener.querySelector('canvas')).not.toBeNull()
  })

  it('draws the last minute under a number that moved, and says which way it went', () => {
    const { rerender } = render(<SignalCard signal={{ ...reading, trace: rising }} />)

    expect(card().querySelector('canvas')).not.toBeNull()
    expect(card()).toHaveAttribute('data-trend', 'up')

    rerender(<SignalCard signal={{ ...reading, trace: falling }} />)
    expect(card()).toHaveAttribute('data-trend', 'down')
  })

  it('draws nothing for a reading that has not moved, or that has no trace', () => {
    const flat = [at(0, 5), at(1, 5), at(2, 5)]
    const { rerender } = render(<SignalCard signal={{ ...reading, trace: flat }} />)

    expect(card().querySelector('canvas')).toBeNull()
    expect(card()).not.toHaveAttribute('data-trend')

    rerender(<SignalCard signal={reading} />)
    expect(card().querySelector('canvas')).toBeNull()
    expect(card()).not.toHaveAttribute('data-trend')
  })

  it('lights the value when it changes, and not when it arrives', () => {
    const { rerender } = render(<SignalCard signal={reading} />)
    expect(card().querySelector('[data-ticked]')).toBeNull()

    rerender(<SignalCard signal={{ ...reading, value: '61,970.02' }} />)

    expect(screen.getByText('61,970.02')).toHaveAttribute('data-ticked')
  })
})
