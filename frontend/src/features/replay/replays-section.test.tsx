import { render, screen } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import type { ReplayIndexEntry } from './replay'
import { ReplaysSection } from './replays-section'

function entry(id: string, margin: number): ReplayIndexEntry {
  return {
    id,
    date: '2026-11-19',
    home: { code: 'MEM', name: 'Memphis Grizzlies' },
    away: { code: 'GS', name: 'Golden State Warriors' },
    durationMs: 20_000,
    final: { home: 5 + margin, away: 5 },
  }
}

const games = ['a', 'b', 'c', 'd', 'e', 'f'].map((id, i) => entry(id, i + 1))

beforeEach(() => {
  // The cards fetch on mount; keep them in the loading state.
  vi.stubGlobal(
    'fetch',
    vi.fn(() => new Promise(() => {}))
  )
})

afterEach(() => {
  vi.unstubAllGlobals()
})

describe('ReplaysSection', () => {
  it('heads the section, explains the replays and shows at most four cards', () => {
    render(<ReplaysSection games={games} />)

    expect(screen.getByRole('heading', { level: 2, name: 'Game replays' })).toBeInTheDocument()
    expect(
      screen.getByText(/Finished NBA games played back play by play, with the breaks shortened\./)
    ).toBeInTheDocument()
    expect(screen.getByText(/Scores from ESPN, prices from Kalshi\./)).toBeInTheDocument()
    expect(screen.getAllByRole('article')).toHaveLength(4)
  })

  it('renders nothing when there are no games', () => {
    const { container } = render(<ReplaysSection games={[]} />)

    expect(container).toBeEmptyDOMElement()
  })
})
