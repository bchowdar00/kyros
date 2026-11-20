import { render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'

import type { ReplayGame, ReplayIndexEntry } from './replay'
import { ReplayCard } from './replay-card'

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

function stubFetch(resolve: boolean) {
  vi.stubGlobal(
    'fetch',
    vi.fn(() =>
      resolve
        ? Promise.resolve({ json: () => Promise.resolve(game), ok: true })
        : new Promise(() => {})
    )
  )
}

afterEach(() => {
  vi.unstubAllGlobals()
  vi.restoreAllMocks()
})

describe('ReplayCard', () => {
  it('shows the running score, quarter, clock, percent and signed change, linked to the replay', async () => {
    vi.spyOn(Date, 'now').mockReturnValue(12_000)
    stubFetch(true)
    render(<ReplayCard entry={entry} index={0} />)

    expect(await screen.findByText('Q2 8:20')).toBeInTheDocument()
    expect(screen.getByText('GS')).toBeInTheDocument()
    expect(screen.getByText('3')).toBeInTheDocument()
    expect(screen.getByText('2')).toBeInTheDocument()
    expect(screen.getByText('MEM 67%')).toBeInTheDocument()
    expect(screen.getByText('+6.0')).toBeInTheDocument()

    expect(
      screen.getByRole('link', { name: 'Golden State Warriors at Memphis Grizzlies' })
    ).toHaveAttribute('href', '/game/g1')
  })

  it('reads Final once the game has ended', async () => {
    vi.spyOn(Date, 'now').mockReturnValue(25_000)
    stubFetch(true)
    render(<ReplayCard entry={entry} index={0} />)

    expect(await screen.findByText('Final')).toBeInTheDocument()
  })

  it('says the replay is unavailable when its file cannot be read, and stops saying loading', async () => {
    vi.spyOn(Date, 'now').mockReturnValue(12_000)
    vi.stubGlobal(
      'fetch',
      vi.fn(() => Promise.resolve({ json: () => Promise.resolve({ message: 'gone' }), ok: false }))
    )
    render(<ReplayCard entry={entry} index={0} />)

    expect(await screen.findByText('Game unavailable')).toBeInTheDocument()
    expect(screen.queryByText('Loading game')).toBeNull()
  })

  it('shows the teams and a loading note before the game has loaded', () => {
    vi.spyOn(Date, 'now').mockReturnValue(12_000)
    stubFetch(false)
    render(<ReplayCard entry={entry} index={0} />)

    expect(screen.getByText('Loading game')).toBeInTheDocument()
    expect(screen.getByText('GS')).toBeInTheDocument()
    expect(screen.getByText('MEM')).toBeInTheDocument()
  })
})
