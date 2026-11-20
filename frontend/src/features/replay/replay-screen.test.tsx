import { render, screen, within } from '@testing-library/react'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import type { ReplayGame, ReplayIndexEntry } from './replay'
import { ReplayScreen } from './replay-screen'

const entry: ReplayIndexEntry = {
  id: 'g1',
  date: '2026-11-19',
  home: { code: 'MEM', name: 'Memphis Grizzlies' },
  away: { code: 'GS', name: 'Golden State Warriors' },
  durationMs: 20_000,
  final: { home: 5, away: 3 },
}

const game: ReplayGame = {
  ...entry,
  plays: [
    { atMs: 0, period: 1, clock: '12:00', home: 0, away: 0, team: null, points: 0 },
    { atMs: 5_000, period: 1, clock: '11:35', home: 2, away: 0, team: 'home', points: 2 },
    { atMs: 12_000, period: 2, clock: '8:20', home: 2, away: 3, team: 'away', points: 3 },
    { atMs: 20_000, period: 3, clock: '4:12', home: 5, away: 3, team: 'home', points: 3 },
  ],
  prices: [
    { atMs: 0, bid: 0.6, ask: 0.62 },
    { atMs: 10_000, bid: 0.66, ask: 0.68 },
  ],
}

beforeEach(() => {
  // jsdom reports a zero-size container, and a chart with no size draws nothing.
  vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockReturnValue({
    bottom: 320,
    height: 320,
    left: 0,
    right: 640,
    toJSON: () => ({}),
    top: 0,
    width: 640,
    x: 0,
    y: 0,
  })
  vi.spyOn(Date, 'now').mockReturnValue(12_000)
  vi.stubGlobal(
    'fetch',
    vi.fn(() => Promise.resolve({ json: () => Promise.resolve(game), ok: true }))
  )
})

afterEach(() => {
  vi.unstubAllGlobals()
  vi.restoreAllMocks()
})

describe('ReplayScreen', () => {
  it('names the game, lists the scoring plays newest first and says nothing can be traded', async () => {
    const { container } = render(<ReplayScreen entry={entry} index={0} />)

    expect(
      await screen.findByRole('heading', {
        level: 1,
        name: 'Golden State Warriors at Memphis Grizzlies',
      })
    ).toBeInTheDocument()

    const plays = within(screen.getByRole('list', { name: 'Scoring plays' })).getAllByRole(
      'listitem'
    )
    expect(plays[0]).toHaveTextContent('GS +3 · Q2 8:20 · 3–2')
    expect(plays[1]).toHaveTextContent('MEM +2 · Q1 11:35 · 0–2')

    expect(screen.getByText(/Play by play, with the score and the price\./)).toBeInTheDocument()
    expect(screen.getByRole('link', { name: 'Back to markets' })).toHaveAttribute('href', '/')

    await expectNoA11yViolations(container)
  })

  it('says the replay is unavailable when its file cannot be read', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(() => Promise.reject(new Error('offline')))
    )

    render(<ReplayScreen entry={entry} index={0} />)

    expect(await screen.findByText('Game unavailable')).toBeInTheDocument()
  })

  it('says up front that it already happened and cannot be traded', () => {
    render(<ReplayScreen entry={entry} index={0} />)

    const header = screen.getByRole('heading', { level: 1 }).parentElement
    expect(header).toHaveTextContent('It already happened: nothing here can be traded.')
  })

  it('does not give away the final score while the game is still loading', () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(() => new Promise(() => undefined))
    )
    const spoiler = { ...entry, final: { away: 103, home: 105 } }

    render(<ReplayScreen entry={spoiler} index={0} />)

    expect(screen.getByText('Loading game')).toBeInTheDocument()
    expect(screen.queryByText('105')).toBeNull()
    expect(screen.queryByText('103')).toBeNull()
    expect(screen.queryByText(/Q1 12:00/)).toBeNull()
  })
})
