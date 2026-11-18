import { render, screen, within } from '@testing-library/react'
import { describe, expect, it } from 'vitest'

import { MarketDetailLayout } from '@/features/market-detail/market-detail-layout'
import { MarketDetailScreen } from '@/features/market-detail/market-detail-screen'
import { MARKET_DETAIL_FIXTURES } from '@/features/market-detail/market-detail.fixture'

describe('MarketDetailLayout', () => {
  it('keeps the market column in document flow beside the sticky rail slot', () => {
    render(
      <MarketDetailLayout
        marketColumn={<p>Market column</p>}
        marketSlug="test-market"
        strategyColumn={<p>Strategy column</p>}
      />
    )

    const marketRegion = screen.getByRole('region', { name: 'Market details' })
    const chatPanel = screen.getByRole('complementary', { name: 'Chat panel' })

    expect(within(marketRegion).getByText('Market column')).toBeInTheDocument()
    expect(within(chatPanel).getByText('Strategy column')).toBeInTheDocument()
    expect(screen.getByRole('main')).toHaveAttribute('data-market-slug', 'test-market')
  })

  it('composes the completed feature sections without placeholders', () => {
    render(<MarketDetailScreen market={MARKET_DETAIL_FIXTURES['nba-bos-la-083126']} />)

    expect(screen.queryByRole('region', { name: /placeholder$/i })).not.toBeInTheDocument()
  })
})
