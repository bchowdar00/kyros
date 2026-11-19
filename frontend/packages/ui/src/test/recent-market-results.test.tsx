import { render, screen } from '@testing-library/react'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { describe, expect, it } from 'vitest'

import { RecentMarketResults } from '../charts/recent-market-results'

describe('RecentMarketResults', () => {
  it('shows only the fourteen latest resolved markets, lettered by side', async () => {
    const sixteen = Array.from({ length: 16 }, (_, index) =>
      index % 2 === 0 ? ('above' as const) : ('below' as const)
    )
    const { container } = render(<RecentMarketResults results={sixteen} />)

    expect(
      screen.getByRole('list', { name: 'Results for the 14 most recent markets' })
    ).toBeInTheDocument()
    const markers = screen.getAllByRole('listitem')
    expect(markers).toHaveLength(14)
    // The two oldest are dropped, so the line still opens on an "above" round.
    expect(markers[0]).toHaveAccessibleName('Market resolved above the price to beat')
    expect(markers[0]).toHaveTextContent('U')
    expect(markers[13]).toHaveAccessibleName('Market resolved below the price to beat')
    expect(markers[13]).toHaveTextContent('D')

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
