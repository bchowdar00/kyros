import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { expectNoA11yViolations } from '@tiny/testing/a11y'
import { describe, expect, it, vi } from 'vitest'

import { SignalCard } from './signal-card'

const signal = {
  id: 'btc.polymarket.prediction_quote.v1:affirmative_bid',
  context: 'Polymarket venue price',
  label: 'Polymarket up bid',
  value: '55¢',
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
    const card = screen.getByRole('button', { name: 'Polymarket up bid, 55¢. Open details' })
    await user.click(card)
    expect(onOpen).toHaveBeenCalledWith(signal.id, card)
    await expectNoA11yViolations(container)
  })
})
