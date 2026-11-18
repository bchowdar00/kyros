import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { expectNoA11yViolations } from '@tiny/testing/a11y'
import { describe, expect, it, vi } from 'vitest'

import type { SignalFieldCard } from '@/features/signals/signal-cards'
import { SignalRow } from './signal-row'

const card: SignalFieldCard = {
  pin: 'btc.coinbase.rsi.v1:rsi_14',
  signalId: 'btc.coinbase.rsi.v1',
  label: 'RSI 14',
  value: '61.23',
  kind: 'number',
  summary: 'Live Coinbase reading. Description coming soon.',
  signalName: 'Coinbase RSI',
  venue: 'Coinbase',
}

describe('SignalRow', () => {
  it('shows the name, value and trace, and toggles the pin', async () => {
    const user = userEvent.setup()
    const onToggle = vi.fn()
    const { container } = render(
      <SignalRow
        card={card}
        history={[
          { time: 1, value: 61 },
          { time: 2, value: 62 },
          { time: 3, value: 61 },
        ]}
        onToggle={onToggle}
        pinned={false}
      />
    )

    expect(screen.getByTitle(card.summary)).toHaveTextContent('RSI 14')
    expect(screen.getByText('61.23')).toBeInTheDocument()
    expect(container.querySelector('canvas')).not.toBeNull()

    const pin = screen.getByRole('button', { name: 'Pin Coinbase RSI RSI 14' })
    expect(pin).toHaveAttribute('aria-pressed', 'false')
    await user.click(pin)
    expect(onToggle).toHaveBeenCalledWith('btc.coinbase.rsi.v1:rsi_14')

    await expectNoA11yViolations(container)
  })

  it('reads as pinned, and draws no trace for a state or a value that has not moved', () => {
    const { container, rerender } = render(
      <SignalRow
        card={{ ...card, kind: 'state', value: 'Quiet' }}
        history={[]}
        onToggle={() => undefined}
        pinned
      />
    )
    expect(screen.getByText('Quiet')).toBeInTheDocument()
    expect(container.querySelector('canvas')).toBeNull()

    rerender(
      <SignalRow
        card={card}
        history={[
          { time: 1, value: 80_188 },
          { time: 2, value: 80_188 },
          { time: 3, value: 80_188 },
        ]}
        onToggle={() => undefined}
        pinned
      />
    )
    expect(container.querySelector('canvas')).toBeNull()
    expect(screen.getByRole('button', { name: 'Unpin Coinbase RSI RSI 14' })).toHaveAttribute(
      'aria-pressed',
      'true'
    )
  })
})
