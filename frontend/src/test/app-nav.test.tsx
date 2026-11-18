import { render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { expectNoA11yViolations } from '@tiny/testing/a11y'
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
    expect(screen.getByRole('link', { name: APP_NAV_FIXTURE.profile.linkLabel })).toHaveAttribute(
      'href',
      APP_NAV_FIXTURE.profile.href
    )
  })

  it('provides an accessible search field', async () => {
    const user = userEvent.setup()

    render(<AppNav {...APP_NAV_FIXTURE} />)

    const search = screen.getByRole('searchbox', { name: APP_NAV_FIXTURE.searchLabel })

    await user.type(search, 'NBA')
    expect(search).toHaveValue('NBA')
  })
})
