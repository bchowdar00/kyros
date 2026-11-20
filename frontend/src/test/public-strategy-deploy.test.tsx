import { cents } from '@kyros/money'
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it, vi } from 'vitest'

import { PublicStrategyDeploy } from '@/features/market-detail/public-strategy-deploy'

describe('PublicStrategyDeploy', () => {
  it('deploys under the name of the card and the side that was chosen', async () => {
    const user = userEvent.setup()
    const submit = vi.fn(async () => true)
    render(
      <PublicStrategyDeploy
        allocation="40"
        deploy={{ availableBalanceCents: cents(100_000), notice: '', reason: undefined, submit }}
        inverse
        templateId="inside-quote"
        title="Inside the Spread"
      />
    )

    await user.click(screen.getByRole('button', { name: 'Deploy Strategy' }))

    expect(submit).toHaveBeenCalledWith({
      allocationCents: cents(4000),
      inverse: true,
      templateId: 'inside-quote',
      title: 'Inverse: Inside the Spread',
    })
  })
})
