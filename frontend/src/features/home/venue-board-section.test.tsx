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
  it('draws a price line on a game whose price has moved, and none on one without a history', () => {
    const [nfl, epl] = board.leagues
    if (nfl === undefined || epl === undefined) throw new Error('missing league')
    const [game] = nfl.games
    if (game === undefined) throw new Error('missing game')
    const spark = [
      { time: 1, value: 0.08 },
      { time: 2, value: 0.09 },
      { time: 3, value: 0.085 },
    ]

    render(
      <VenueBoardSection board={{ leagues: [{ ...nfl, games: [{ ...game, spark }] }, epl] }} />
    )

    const moved = screen.getByRole('article', { name: 'Arizona vs Kansas City' })
    const still = screen.getByRole('article', { name: 'Manchester City vs Fulham' })
    expect(moved.querySelector('canvas')).not.toBeNull()
    expect(still.querySelector('canvas')).toBeNull()
  })

  it('shows the heading and says nothing about what cannot be traded', () => {
    render(<VenueBoardSection board={board} />)

    expect(screen.getByRole('heading', { level: 2, name: 'More sports' })).toBeVisible()
    expect(screen.queryByText(/not to trade|to watch/i)).toBeNull()
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
    // One exchange: the card is tagged with its league, never with where the price is read.
    expect(screen.queryByText(/Kalshi|Gemini/)).toBeNull()

    const soccer = screen.getByRole('article', { name: 'Manchester City vs Fulham' })
    expect(within(soccer).getByText('Tie')).toBeVisible()
    expect(within(soccer).getByText('17.5%')).toBeVisible()
  })

  it('renders no link or button on a game nothing can be traded on', () => {
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
