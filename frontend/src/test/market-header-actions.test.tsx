import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { describe, expect, it } from 'vitest'

import { MarketHeaderActions } from '@/features/market-detail/market-header-actions'

describe('MarketHeaderActions', () => {
  it('copies the current market link and announces success', async () => {
    const user = userEvent.setup()
    render(<MarketHeaderActions />)

    await user.click(screen.getByRole('button', { name: 'Share market' }))

    expect(await navigator.clipboard.readText()).toBe(window.location.href)
    expect(screen.getByRole('status')).toHaveTextContent('Market link copied')
  })

  it('toggles the favorite with the keyboard and remains accessible', async () => {
    const user = userEvent.setup()
    const { container } = render(<MarketHeaderActions />)
    const favorite = screen.getByRole('button', { name: 'Favorite market' })

    await user.tab()
    await user.tab()
    expect(favorite).toHaveFocus()
    await user.keyboard(' ')
    expect(favorite).toHaveAttribute('aria-pressed', 'true')
    await user.keyboard(' ')
    expect(favorite).toHaveAttribute('aria-pressed', 'false')
    await expectNoA11yViolations(container)
  })
})
