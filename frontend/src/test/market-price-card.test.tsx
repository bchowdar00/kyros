import { render, screen } from '@testing-library/react'
import { expectNoA11yViolations } from '@tiny/testing/a11y'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { getMarketDetailFixture } from '@/features/market-detail/market-detail.fixture'
import { MarketPriceCard } from '@/features/market-detail/market-price-card'

const market = getMarketDetailFixture('btc-above-118k-0903')

if (!market?.priceChart) {
  throw new Error('The bitcoin fixture is expected to carry a price chart.')
}

const priceChart = market.priceChart

describe('MarketPriceCard', () => {
  beforeEach(() => {
    // jsdom reports a zero-size container, and a chart with no size draws nothing.
    vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockReturnValue({
      bottom: 493,
      height: 493,
      left: 0,
      right: 1110,
      toJSON: () => ({}),
      top: 0,
      width: 1110,
      x: 0,
      y: 0,
    })
  })

  afterEach(() => {
    vi.restoreAllMocks()
  })

  it('leads with the target, current price, change and time remaining', async () => {
    vi.spyOn(Date, 'now').mockReturnValue(priceChart.range.end - 60_000)
    const { container } = render(<MarketPriceCard {...priceChart} />)

    expect(screen.getByText('Current')).toBeInTheDocument()
    expect(screen.queryByText('(0.1%)')).not.toBeInTheDocument()
    expect(screen.getByText('Change · feed paused')).toBeInTheDocument()
    expect(screen.getByText('+$120.00')).toBeInTheDocument()
    expect(screen.getByText('Time Left')).toBeInTheDocument()
    expect(screen.getByText('01:00')).toBeInTheDocument()
    expect(screen.getAllByText('$118,120.00')[0]).toBeInTheDocument()

    // Once as the headline's term, once as the chart's reference-line pill.
    expect(screen.getAllByText('Price to beat')).toHaveLength(2)
    expect(screen.getByText('$118,000.00')).toBeInTheDocument()
    const fallbackResults = screen.getByRole('list', {
      name: 'Recent market results unavailable',
    })
    expect(fallbackResults).toBeInTheDocument()
    expect(screen.getAllByRole('listitem')).toHaveLength(5)

    await expectNoA11yViolations(container)
  })

  it('marks the price flag and the window on the chart', () => {
    const { container } = render(<MarketPriceCard {...priceChart} />)

    // The flag uses the latest observed price; the card never generates a fallback tick.
    expect(container.querySelector('[data-chart-value-flag]')?.textContent).toBe('$118,120.00')
    expect(container.querySelector('[data-chart-reference-line]')).toBeInTheDocument()
  })

  it('says so when the market is not live', () => {
    render(<MarketPriceCard {...priceChart} live={false} />)

    expect(screen.getByText('Change · feed paused')).toBeInTheDocument()
  })
})
