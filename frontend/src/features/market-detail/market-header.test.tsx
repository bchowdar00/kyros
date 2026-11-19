import { render, screen } from '@testing-library/react'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { describe, expect, it } from 'vitest'

import { MarketHeader } from './market-header'
import type { MarketOutcome } from './types'

const unquoted: readonly [MarketOutcome, MarketOutcome] = [
  { side: 'home', label: 'UP' },
  { side: 'away', label: 'DOWN' },
]
const header = {
  title: 'Test market',
  subtitle: '',
  surfaceImage: { src: '/assets/market/header-surface.png', alt: '' },
  marketSlug: 'test-market',
}

describe('market buy controls', () => {
  it('keeps both outcomes visible without quotes and enables them when quotes arrive', async () => {
    const { container, rerender } = render(<MarketHeader {...header} outcomes={unquoted} />)
    for (const label of ['UP', 'DOWN']) {
      const button = screen.getByRole('button', { name: `Buy ${label} outcome` })
      expect(button).toBeVisible()
      expect(button).toBeDisabled()
      expect(button).toHaveAccessibleDescription('Price unavailable')
      expect(button).toHaveTextContent('—')
    }
    await expectNoA11yViolations(container)
    rerender(
      <MarketHeader
        {...header}
        outcomes={[
          { ...unquoted[0], priceTicks: 490 },
          { ...unquoted[1], priceTicks: 520 },
        ]}
      />
    )
    expect(screen.getByRole('button', { name: /Buy UP outcome/ })).toBeEnabled()
    expect(screen.getByRole('button', { name: /Buy DOWN outcome/ })).toBeEnabled()
  })

  it('renders Buy UP before Buy DOWN, in the order the catalogue mapper supplies', () => {
    render(<MarketHeader {...header} outcomes={unquoted} />)
    const labels = screen
      .getAllByRole('button', { name: /Buy (UP|DOWN) outcome/ })
      .map(button => button.getAttribute('aria-label'))
    expect(labels).toEqual(['Buy UP outcome', 'Buy DOWN outcome'])
  })

  it('disables only the outcome missing depth', () => {
    render(
      <MarketHeader {...header} outcomes={[{ ...unquoted[0], priceCents: 52 }, unquoted[1]]} />
    )
    expect(screen.getByRole('button', { name: /Buy DOWN outcome/ })).toBeDisabled()
    expect(screen.getByRole('button', { name: /Buy UP outcome/ })).toBeEnabled()
  })

  it('keeps quoted controls disabled with a visible connection reason', () => {
    render(
      <MarketHeader
        {...header}
        outcomes={[
          { ...unquoted[0], priceTicks: 490 },
          { ...unquoted[1], priceTicks: 520 },
        ]}
        tradingUnavailableReason="Exchange feed disconnected. Waiting for a fresh book."
      />
    )
    expect(screen.getByText('Exchange feed disconnected. Waiting for a fresh book.')).toBeVisible()
    for (const label of ['UP', 'DOWN']) {
      expect(
        screen.getByRole('button', { name: new RegExp(`Buy ${label} outcome`) })
      ).toBeDisabled()
    }
  })
})
