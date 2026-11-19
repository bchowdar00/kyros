import { render, screen } from '@testing-library/react'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { describe, expect, it } from 'vitest'

import { MarketPayoutPreview } from './market-payout-preview'

describe('MarketPayoutPreview', () => {
  it('updates the return from live sub-cent quotes', () => {
    const { rerender } = render(<MarketPayoutPreview outcomeLabel="UP" priceTicks={501} />)
    expect(screen.getByRole('status', { name: 'Potential payout for UP' })).toHaveTextContent(
      '$100could return $200 if UP wins'
    )
    rerender(<MarketPayoutPreview outcomeLabel="UP" priceTicks={625} />)
    expect(screen.getByRole('status', { name: 'Potential payout for UP' })).toHaveTextContent(
      '$100could return $160 if UP wins'
    )
  })

  it.each([
    [38, '$263'],
    [62, '$161'],
    [50, '$200'],
  ] as const)('shows a whole-dollar return at %i cents', async (priceCents, payout) => {
    const { container } = render(<MarketPayoutPreview outcomeLabel="UP" priceCents={priceCents} />)
    const preview = screen.getByRole('status', { name: 'Potential payout for UP' })
    expect(preview).toHaveTextContent(`$100could return ${payout} if UP wins`)
    await expectNoA11yViolations(container)
  })

  it.each([undefined, 0, -1, 101, Number.NaN])(
    'keeps the return unknown for an invalid price (%s)',
    priceCents => {
      render(<MarketPayoutPreview outcomeLabel="DOWN" priceCents={priceCents} />)
      expect(screen.getByRole('status', { name: 'Potential payout for DOWN' })).toHaveTextContent(
        '$100could return — if DOWN wins'
      )
    }
  )
})
