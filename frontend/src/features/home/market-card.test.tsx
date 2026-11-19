import { render, screen } from '@testing-library/react'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { describe, expect, it } from 'vitest'

import { MarketCard } from './market-card'

describe('MarketCard', () => {
  it('says the exchange’s capital side labels the way the market page does', async () => {
    const { container } = render(
      <MarketCard
        category="Crypto"
        chart={null}
        outcomes={[
          { label: 'UP', side: 'home' },
          { label: 'DOWN', priceCents: 61, side: 'away' },
        ]}
        slug="btc-5m"
        title="BTC Up or Down 5m"
      />
    )

    expect(screen.getByText('Up')).toBeVisible()
    expect(screen.getByText('Down 61¢')).toBeVisible()
    expect(screen.queryByText('UP')).not.toBeInTheDocument()

    await expectNoA11yViolations(container)
  })

  it('leaves a team name as the exchange wrote it', () => {
    render(
      <MarketCard
        category="NBA"
        chart={null}
        outcomes={[
          { label: 'LAL', side: 'home' },
          { label: 'BOS', side: 'away' },
        ]}
        slug="nba-h2h"
        title="Celtics at Lakers"
      />
    )

    expect(screen.getByText('LAL')).toBeVisible()
    expect(screen.getByText('BOS')).toBeVisible()
  })
})
