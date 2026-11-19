import { fireEvent, render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { describe, expect, it, vi } from 'vitest'

import { SliderExample } from '@/features/chat/examples/interactive-examples'
import { Picker } from '@/features/chat/widgets/picker'
import { TicketForm } from '@/features/chat/widgets/ticket-form'

describe('Picker widget', () => {
  it('sends the chosen option once and shows it', async () => {
    const user = userEvent.setup()
    const onSelect = vi.fn()
    const { container } = render(
      <Picker
        label="Choose a version"
        prompt="Deploy which one?"
        options={[
          { id: 'v1', title: 'V1' },
          { id: 'v2', title: 'V2' },
        ]}
        onSelect={onSelect}
      />
    )
    await expectNoA11yViolations(container)
    expect(screen.getByRole('button', { name: 'Continue' })).toBeDisabled()
    await user.click(screen.getByRole('radio', { name: 'V2' }))
    await user.click(screen.getByRole('button', { name: 'Continue' }))
    expect(onSelect).toHaveBeenCalledExactlyOnceWith('v2')
    expect(screen.getByRole('status')).toHaveTextContent('V2')
  })
})

describe('ScenarioSlider widget', () => {
  it('recomputes the readouts as the value moves', async () => {
    const { container } = render(<SliderExample />)
    const slider = screen.getByRole('slider', { name: 'Move from strike at close' })
    expect(slider).toHaveValue('40')
    expect(screen.getByText('YES')).toBeVisible()
    fireEvent.change(slider, { target: { value: '-120' } })
    expect(screen.getByText('NO')).toBeVisible()
    expect(screen.getByText('-$58.00')).toBeVisible()
    await expectNoA11yViolations(container)
  })
})

describe('TicketForm widget', () => {
  it('quotes the payout, sends one order and confirms', async () => {
    const user = userEvent.setup()
    const onSubmit = vi.fn()
    const { container } = render(
      <TicketForm label="Trade ticket" prices={{ YES: 58, NO: 43 }} onSubmit={onSubmit} />
    )
    await expectNoA11yViolations(container)
    expect(screen.getByText('$43.10')).toBeVisible()
    await user.click(screen.getByRole('radio', { name: /NO/ }))
    await user.click(screen.getByRole('button', { name: '$50' }))
    expect(screen.getByText('$116.28')).toBeVisible()
    await user.click(screen.getByRole('button', { name: 'Buy NO' }))
    expect(onSubmit).toHaveBeenCalledWith({ side: 'NO', amountCents: 5_000, priceCents: 43 })
    expect(screen.getByRole('status')).toHaveTextContent('Sent. Buy NO for $50.00 at 43¢.')
  })

  it('fails closed with the reason on the button', () => {
    render(
      <TicketForm
        label="Trade ticket"
        prices={{ YES: 58, NO: 43 }}
        disabledReason="Exchange unavailable"
        onSubmit={vi.fn()}
      />
    )
    expect(screen.getByRole('button', { name: 'Exchange unavailable' })).toBeDisabled()
  })
})
