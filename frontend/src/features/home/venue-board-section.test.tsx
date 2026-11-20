import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { render, screen, within } from '@testing-library/react'
import { describe, expect, it } from 'vitest'

import type { VenueBoard } from './venue-board'
import { VenueBoardSection } from './venue-board-section'

const board: VenueBoard = {
  leagues: [
    {
      label: 'NFL',
      games: [
        {
          id: 'KXNFLGAME-26NOV22ARIKC',
          title: 'Arizona vs Kansas City',
          when: 'Nov 22',
          outcomes: [
            { label: 'Arizona', percent: 8.5 },
            { label: 'Kansas City', percent: 91.5 },
          ],
          volume24h: 55481.84,
        },
      ],
    },
    {
      label: 'Premier League',
      games: [
        {
          id: 'KXEPLGAME-26NOV21MCIFUL',
          title: 'Manchester City vs Fulham',
          when: undefined,
          outcomes: [
            { label: 'Manchester City', percent: 70.5 },
            { label: 'Tie', percent: 17.5 },
          ],
          volume24h: 61094.27,
        },
      ],
    },
  ],
}

describe('VenueBoardSection', () => {
  it('shows the heading and the watch-only caption', () => {
    render(<VenueBoardSection board={board} />)

    expect(
      screen.getByRole('heading', { level: 2, name: 'More games, priced on Kalshi' })
    ).toBeVisible()
    expect(
      screen.getByText(
        "Gemini does not list these yet. The prices are Kalshi's, shown here to watch, not to trade."
      )
    ).toBeVisible()
  })

  it('lists each league with its games and every outcome and price', () => {
    render(<VenueBoardSection board={board} />)

    expect(screen.getByRole('heading', { level: 3, name: 'NFL' })).toBeVisible()
    expect(screen.getByRole('heading', { level: 3, name: 'Premier League' })).toBeVisible()

    const game = screen.getByRole('article', { name: 'Arizona vs Kansas City' })
    expect(
      within(game).getByRole('heading', { level: 4, name: 'Arizona vs Kansas City' })
    ).toBeVisible()
    expect(within(game).getByText('Nov 22')).toBeVisible()
    expect(within(game).getByText('Arizona')).toBeVisible()
    expect(within(game).getByText('8.5%')).toBeVisible()
    expect(within(game).getByText('Kansas City')).toBeVisible()
    expect(within(game).getByText('91.5%')).toBeVisible()
    expect(within(game).getByText('Kalshi')).toBeVisible()

    const soccer = screen.getByRole('article', { name: 'Manchester City vs Fulham' })
    expect(within(soccer).getByText('Tie')).toBeVisible()
    expect(within(soccer).getByText('17.5%')).toBeVisible()
  })

  it('renders no link or button: the games are to watch, not to trade', () => {
    render(<VenueBoardSection board={board} />)

    expect(screen.queryByRole('link')).toBeNull()
    expect(screen.queryByRole('button')).toBeNull()
  })

  it('never names the assistant in its copy', () => {
    const { container } = render(<VenueBoardSection board={board} />)

    expect(container.textContent).not.toMatch(/Claude|Tiny|Teeny/)
  })

  it('has no accessibility violations', async () => {
    const { container } = render(<VenueBoardSection board={board} />)

    await expectNoA11yViolations(container)
  })
})
