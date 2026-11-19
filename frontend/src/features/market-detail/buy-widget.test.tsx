import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { describe, expect, it } from 'vitest'

import { ChatPanel } from './chat-panel'
import type { MarketTrading } from './exchange/buy'
import { MarketHeader } from './market-header'
import type { MarketOutcome } from './types'

const outcomes: readonly [MarketOutcome, MarketOutcome] = [
  { side: 'home', label: 'UP', priceCents: 31 },
  { side: 'away', label: 'DOWN', priceCents: 69 },
]

function renderMarket(marketKey: string, trading?: MarketTrading) {
  return render(
    <>
      <MarketHeader
        outcomes={outcomes}
        title="Test market"
        subtitle=""
        surfaceImage={{ src: '/assets/market/header-surface.png', alt: '' }}
        marketSlug={marketKey}
      />
      <ChatPanel
        trading={trading}
        marketKey={marketKey}
        outcomes={outcomes}
        data={{ publicStrategies: [], view: 'strategies' }}
      />
    </>
  )
}

describe('composer to buy widget', () => {
  it.each(['UP', 'DOWN'])(
    'opens with %s selected and restores the draft and trigger on Escape',
    async label => {
      const user = userEvent.setup()
      const { container } = renderMarket(`select-${label}`, {
        availableBalanceCents: undefined,
        reason: 'Waiting for the exchange account snapshot.',
        notice: '',
        quote: () => undefined,
        submit: () => {},
      })
      const composer = screen.getByRole('textbox', { name: 'Message Kyros' })
      await user.type(composer, 'Keep my strategy draft')
      const trigger = screen.getByRole('button', { name: new RegExp(`Buy ${label} outcome`) })
      await user.click(trigger)
      const widget = screen.getByRole('dialog', { name: 'Buy outcome' })
      expect(within(widget).getByRole('button', { name: `Select ${label}` })).toHaveAttribute(
        'aria-pressed',
        'true'
      )
      expect(screen.queryByRole('form', { name: 'Strategy composer' })).not.toBeInTheDocument()
      await user.type(within(widget).getByRole('textbox', { name: 'Order amount in USD' }), '25.50')
      expect(within(widget).getByRole('textbox')).toHaveValue('$25.50')
      expect(within(widget).getByRole('button', { name: 'Buy' })).toBeDisabled()
      await expectNoA11yViolations(container)
      await user.keyboard('{Escape}')
      await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument())
      expect(composer).toHaveValue('Keep my strategy draft')
      expect(trigger).toHaveFocus()
    }
  )

  it('switches outcomes and dismisses through the dimmed panel', async () => {
    const user = userEvent.setup()
    renderMarket('switch-outcome')
    await user.click(screen.getByRole('button', { name: /Buy UP outcome/ }))
    await user.click(screen.getByRole('button', { name: 'Select DOWN' }))
    expect(screen.getByRole('button', { name: 'Select DOWN' })).toHaveAttribute(
      'aria-pressed',
      'true'
    )
    expect(screen.getByRole('button', { name: 'Select UP' })).toHaveAttribute(
      'aria-pressed',
      'false'
    )
    await user.click(screen.getByRole('button', { name: /Buy UP outcome/ }))
    expect(screen.getByRole('button', { name: 'Select UP' })).toHaveAttribute(
      'aria-pressed',
      'true'
    )
    await user.keyboard('{Escape}')
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument())
    await user.click(screen.getByRole('button', { name: /Buy UP outcome/ }))
    await user.click(screen.getByRole('button', { name: 'Close buy widget' }))
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument())
    expect(screen.getByRole('textbox', { name: 'Message Kyros' })).toBeVisible()
  })
})
