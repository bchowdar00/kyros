import { render, screen, within } from '@testing-library/react'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { describe, expect, it } from 'vitest'

import { AppNav } from '@/components/app-nav/app-nav'
import { APP_NAV_FIXTURE } from '@/components/app-nav/app-nav.fixture'

describe('AppNav', () => {
  it('renders the primary destinations and account summary', async () => {
    const { container } = render(<AppNav {...APP_NAV_FIXTURE} />)
    await expectNoA11yViolations(container)

    const navigation = screen.getByRole('navigation', { name: 'Primary' })

    for (const item of APP_NAV_FIXTURE.items) {
      expect(within(navigation).getByRole('link', { name: item.label })).toHaveAttribute(
        'href',
        item.href
      )
    }

    expect(screen.getByText(APP_NAV_FIXTURE.balanceLabel)).toBeInTheDocument()
    expect(screen.getByText(APP_NAV_FIXTURE.balance)).toBeInTheDocument()
    expect(screen.getByRole('link', { name: 'Kyros, home' })).toHaveAttribute('href', '/')
  })

  it('links only to screens that exist', () => {
    render(<AppNav {...APP_NAV_FIXTURE} />)

    const navigation = screen.getByRole('navigation', { name: 'Primary' })
    expect(
      within(navigation)
        .getAllByRole('link')
        .map(link => link.getAttribute('href'))
    ).toEqual(['/', '/', '/chat'])
    expect(screen.queryByRole('searchbox')).toBeNull()
  })

  it('offers a theme switch and the account menu it is given', () => {
    render(<AppNav {...APP_NAV_FIXTURE} profileMenu={<button type="button">Account</button>} />)

    expect(screen.getByRole('button', { name: /^Switch to the (dark|light) theme$/ })).toBeVisible()
    expect(screen.getByRole('button', { name: 'Account' })).toBeVisible()
  })

  it('shows the site mode with its description when given', async () => {
    const mode = {
      label: 'Paper',
      description:
        "Pretend money, filled against Gemini's live order book. No order leaves this site.",
      tone: 'neutral',
    } as const
    const { container } = render(<AppNav {...APP_NAV_FIXTURE} mode={mode} />)

    const navigation = screen.getByRole('navigation', { name: 'Primary' })
    expect(within(navigation).getByText('Paper')).toBeInTheDocument()
    expect(within(navigation).getByText(mode.description)).toBeInTheDocument()
    await expectNoA11yViolations(container)
  })

  it('shows no site mode when none is given', () => {
    render(<AppNav {...APP_NAV_FIXTURE} />)

    expect(screen.queryByText('Paper')).toBeNull()
    expect(screen.queryByText(/Pretend money/)).toBeNull()
  })
})
