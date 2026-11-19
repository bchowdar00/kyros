import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { cents } from '@kyros/money'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { useState } from 'react'
import { describe, expect, it } from 'vitest'

import { BuyAmountInput } from './buy-amount-input'
import { BuyPayoutSummary } from './buy-payout-summary'

function AmountHarness() {
  const [amount, setAmount] = useState('0.10')
  return (
    <BuyAmountInput
      amount={amount}
      onAmountChange={setAmount}
      availableBalanceCents={cents(12_345)}
    />
  )
}

describe('buy amount controls', () => {
  it('adds each preset precisely and sets Max from the supplied balance', async () => {
    const user = userEvent.setup()
    const { container } = render(<AmountHarness />)
    const input = screen.getByRole('textbox', { name: 'Order amount in USD' })
    for (const increment of ['+$1', '+$5', '+$10', '+$25']) {
      await user.click(screen.getByRole('button', { name: increment }))
    }
    expect(input).toHaveValue('$41.10')
    await user.click(screen.getByRole('button', { name: 'Max' }))
    expect(input).toHaveValue('$123.45')
    expect(screen.queryByRole('slider')).not.toBeInTheDocument()
    await expectNoA11yViolations(container)
  })

  it('disables Max without a balance and distinguishes unknown payout from zero', () => {
    const { rerender } = render(
      <>
        <BuyAmountInput amount="" onAmountChange={() => {}} />
        <BuyPayoutSummary />
      </>
    )
    expect(screen.getByRole('button', { name: 'Max' })).toBeDisabled()
    expect(screen.getByText('You win if right')).toBeInTheDocument()
    expect(screen.getByRole('status', { name: 'Potential payout' })).toHaveTextContent('Unknown')
    rerender(<BuyPayoutSummary payoutCents={cents(0)} averagePriceCents={cents(31)} />)
    expect(screen.getByRole('status', { name: 'Potential payout' })).toHaveTextContent('$0.00')
    expect(screen.getByText('31¢')).toBeInTheDocument()
  })
})
