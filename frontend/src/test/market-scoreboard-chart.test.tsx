import { render, screen, within } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { MarketProbabilityCard } from '@/features/market-detail/market-probability-card'
import { MarketScoreboard } from '@/features/market-detail/market-scoreboard'
import { MARKET_DETAIL_FIXTURES } from '@/features/market-detail/market-detail.fixture'

const market = MARKET_DETAIL_FIXTURES['nba-bos-la-083126']

describe('MarketScoreboard', () => {
  it('renders both teams around the period and game clock', () => {
    render(<MarketScoreboard scoreboard={market.scoreboard} teams={market.header.teams} />)

    const scoreboard = screen.getByRole('region', { name: 'Market scoreboard' })

    expect(within(scoreboard).getByText('BOS')).toBeInTheDocument()
    expect(within(scoreboard).getByText('90')).toBeInTheDocument()
    expect(within(scoreboard).getByText('Q4')).toBeInTheDocument()
    expect(within(scoreboard).getByText('01:58')).toBeInTheDocument()
    expect(within(scoreboard).getByText('LAL')).toBeInTheDocument()
    expect(within(scoreboard).getByText('95')).toBeInTheDocument()
  })
})

describe('MarketProbabilityCard', () => {
  beforeEach(() => {
    vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockReturnValue({
      bottom: 292,
      height: 292,
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

  it('renders the fixture timeline through the shared market chart', () => {
    const { container } = render(<MarketProbabilityCard {...market.probabilityChart} />)

    expect(
      screen.getByRole('figure', { name: 'probability over time: BOS 31%, LAL 69%' })
    ).toBeInTheDocument()
    expect(market.probabilityChart.series[0].points).toHaveLength(10_000)
    expect(container.querySelectorAll('[data-chart-series]')).toHaveLength(2)
    expect(container.querySelector('[data-chart-series="away"]')).toBeInTheDocument()
    expect(container.querySelector('[data-chart-series="home"]')).toBeInTheDocument()

    // A dense feed draws as one monotone cubic, not a polyline through every tick.
    for (const path of container.querySelectorAll('[data-chart-series-line]')) {
      expect(path.getAttribute('d')).toMatch(/^M[\d.]+ [\d.]+C/)
      expect(path.getAttribute('d')).not.toContain('L')
    }

    expect(screen.getByText('100%')).toBeInTheDocument()
    expect(screen.getByText('0%')).toBeInTheDocument()
    const localClock = new Intl.DateTimeFormat('en-US', { hour: 'numeric', minute: '2-digit' })
    for (const minutes of [0, 45, 90]) {
      const instant = market.probabilityChart.range.start + minutes * 60_000
      const label = localClock.format(instant).replaceAll('\u202f', ' ')
      expect(screen.getByText(label)).toBeInTheDocument()
    }
    expect(screen.getByRole('slider')).toBeInTheDocument()
  })
})
