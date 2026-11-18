import { render, screen } from '@testing-library/react'
import { expectNoA11yViolations } from '@tiny/testing/a11y'
import { describe, expect, it } from 'vitest'

import { RecentMarketResults } from '../charts/recent-market-results'

describe('RecentMarketResults', () => {
  it('shows only the five latest resolved markets', async () => {
    const { container } = render(
      <RecentMarketResults results={['above', 'below', 'above', 'below', 'above', 'below']} />
    )

    expect(
      screen.getByRole('list', { name: 'Results for the 5 most recent markets' })
    ).toBeInTheDocument()
    expect(screen.getAllByRole('listitem').map(item => item.getAttribute('aria-label'))).toEqual([
      'Market resolved below the price to beat',
      'Market resolved above the price to beat',
      'Market resolved below the price to beat',
      'Market resolved above the price to beat',
      'Market resolved below the price to beat',
    ])

    await expectNoA11yViolations(container)
  })

  it('uses five alternating markers without claiming unavailable results', async () => {
    const { container } = render(<RecentMarketResults />)

    expect(
      screen.getByRole('list', { name: 'Recent market results unavailable' })
    ).toBeInTheDocument()
    expect(screen.getAllByRole('listitem').map(item => item.getAttribute('aria-label'))).toEqual([
      'Market result unavailable',
      'Market result unavailable',
      'Market result unavailable',
      'Market result unavailable',
      'Market result unavailable',
    ])

    await expectNoA11yViolations(container)
  })
})
