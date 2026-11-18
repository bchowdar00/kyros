import { render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { ChartPlaygroundScreen } from '@/features/chart-playground/chart-playground-screen'
import {
  ETH_MARKET_FIXTURE,
  SPORTS_MARKET_FIXTURE,
} from '@/features/chart-playground/chart-playground.fixture'

function region(name: string) {
  return screen.getByRole('region', { name })
}

describe('ChartPlaygroundScreen', () => {
  beforeEach(() => {
    vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockReturnValue({
      bottom: 360,
      height: 360,
      left: 0,
      right: 1255,
      toJSON: () => ({}),
      top: 0,
      width: 1255,
      x: 0,
      y: 0,
    })
  })

  afterEach(() => {
    vi.restoreAllMocks()
  })

  it('loads the probability and candlestick comparisons when requested', async () => {
    const user = userEvent.setup()
    render(<ChartPlaygroundScreen />)

    const btc = region('Bitcoin Up or Down · Demo')
    expect(within(btc).queryByText('Current')).not.toBeInTheDocument()
    expect(within(btc).queryByText('Time Left')).not.toBeInTheDocument()
    expect(within(btc).getAllByText('$75,674.18')[0]).toBeInTheDocument()
    expect(btc.querySelector('[data-chart-kind="price"]')).toBeInTheDocument()
    expect(within(btc).getByText('Price to beat')).toBeInTheDocument()
    expect(within(btc).getAllByText('$75,674.18').length).toBeGreaterThan(0)

    await user.click(screen.getByRole('button', { name: 'Show probability and candlestick demos' }))
    const sports = await screen.findByRole('region', { name: SPORTS_MARKET_FIXTURE.title })
    expect(sports.querySelector('[data-chart-kind="probability"]')).toBeInTheDocument()
    expect(sports.querySelectorAll('[data-chart-series]')).toHaveLength(2)
    expect(within(sports).getByText('Halftime')).toBeInTheDocument()
    expect(within(sports).getByText('Q4')).toBeInTheDocument()
    expect(within(sports).getAllByText('31%').length).toBeGreaterThan(0)
    expect(within(sports).getAllByText('69%').length).toBeGreaterThan(0)

    const eth = region(ETH_MARKET_FIXTURE.title)
    expect(eth.querySelector('[data-chart-kind="candlestick"]')).toBeInTheDocument()
    expect(eth.querySelectorAll('[data-chart-candle]').length).toBeGreaterThan(10)
    expect(within(eth).queryByText('Current')).not.toBeInTheDocument()
    expect(within(eth).getByText('Price to beat')).toBeInTheDocument()
    expect(
      within(eth).queryByRole('radio', { name: 'Market probability chart' })
    ).not.toBeInTheDocument()
  })

  it('switches the BTC market between asset price and market probability with one control', async () => {
    const user = userEvent.setup()
    render(<ChartPlaygroundScreen />)
    const btc = region('Bitcoin Up or Down · Demo')

    expect(within(btc).queryByRole('tab', { name: 'Price' })).not.toBeInTheDocument()
    expect(within(btc).queryByRole('tab', { name: 'Candles' })).not.toBeInTheDocument()
    expect(within(btc).queryByRole('tab', { name: 'Probability' })).not.toBeInTheDocument()

    const probabilityToggle = within(btc).getByRole('radio', { name: 'Market probability chart' })
    expect(probabilityToggle).not.toBeChecked()

    await user.click(probabilityToggle)
    expect(btc.querySelector('[data-chart-kind="probability"]')).toBeInTheDocument()
    expect(probabilityToggle).toBeChecked()
    expect(btc.querySelectorAll('[data-chart-series]')).toHaveLength(2)
    expect(within(btc).getByText('100%')).toBeInTheDocument()
    expect(within(btc).queryByText('Price to beat')).not.toBeInTheDocument()

    const assetToggle = within(btc).getByRole('radio', { name: 'BTC price chart' })
    await user.click(assetToggle)
    expect(btc.querySelector('[data-chart-kind="price"]')).toBeInTheDocument()
    expect(assetToggle).toBeChecked()
  })

  it('runs every simulated feed by default without feed or time controls', () => {
    render(<ChartPlaygroundScreen />)

    expect(screen.queryByRole('button', { name: 'Stop live feed' })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: 'Simulate live feed' })).not.toBeInTheDocument()
    expect(screen.queryByRole('radio', { name: '1 Hour' })).not.toBeInTheDocument()
  })
})
