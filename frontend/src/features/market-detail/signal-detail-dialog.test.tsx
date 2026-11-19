import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { describe, expect, it, vi } from 'vitest'

import type { SignalFieldCard } from '@/features/signals/signal-cards'
import { SignalDetailDialog } from './signal-detail-dialog'

const card: SignalFieldCard = {
  pin: 'btc.coinbase.liquidity_shock.v1:depth_ratio_60s',
  signalId: 'btc.coinbase.liquidity_shock.v1',
  label: 'Depth vs 60s',
  value: '+15.7%',
  kind: 'number',
  summary: 'How much the book has thinned or widened in the last minute.',
  description:
    'Size resting near the touch now, as a change against its one-minute average. Negative means the book has thinned.',
  signalName: 'Liquidity shock',
  venue: 'Coinbase',
}

const moving = [
  { time: 1, value: 1 },
  { time: 2, value: 1.1 },
  { time: 3, value: 1.15 },
]

describe('SignalDetailDialog', () => {
  it('explains the number, draws its trace, and pins it', async () => {
    const user = userEvent.setup()
    const onToggle = vi.fn()
    const { container } = render(
      <SignalDetailDialog
        card={card}
        history={moving}
        onClose={() => undefined}
        onToggle={onToggle}
        pinned={false}
      />
    )

    const dialog = screen.getByRole('dialog', { name: 'Depth vs 60s' })
    expect(dialog).toHaveTextContent('Liquidity shock on Coinbase')
    expect(dialog).toHaveTextContent('+15.7%')
    expect(dialog).toHaveTextContent('Negative means the book has thinned.')
    expect(dialog).toHaveTextContent('How much the book has thinned or widened in the last minute.')
    expect(container.querySelector('canvas')).not.toBeNull()
    expect(screen.getByRole('button', { name: 'Close Depth vs 60s' })).toHaveFocus()

    await user.click(screen.getByRole('button', { name: 'Pin to strip' }))
    expect(onToggle).toHaveBeenCalledWith(card.pin)

    await expectNoA11yViolations(container)
  })

  it('falls back to the signal summary, says why there is no trace, and closes from the backdrop', async () => {
    const user = userEvent.setup()
    const onClose = vi.fn()
    const { description: _dropped, ...withoutDescription } = card
    const { container } = render(
      <SignalDetailDialog
        card={{ ...withoutDescription, kind: 'state', value: 'Normal' }}
        history={[]}
        onClose={onClose}
        onToggle={() => undefined}
        pinned
      />
    )
    expect(
      screen.getByText('How much the book has thinned or widened in the last minute.')
    ).toBeInTheDocument()
    expect(screen.getByText('A state, not a number, so there is no trace.')).toBeInTheDocument()
    expect(container.querySelector('canvas')).toBeNull()
    expect(screen.getByRole('button', { name: 'Pinned to strip' })).toHaveAttribute(
      'aria-pressed',
      'true'
    )

    await user.click(screen.getByRole('button', { name: 'Close details' }))
    expect(onClose).toHaveBeenCalled()
  })
})
