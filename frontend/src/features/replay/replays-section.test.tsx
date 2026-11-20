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

const games = ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j'].map((id, i) => entry(id, i + 1))

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
  it('heads the section as games and shows at most eight cards', () => {
    render(<ReplaysSection games={games} />)

    expect(screen.getByRole('heading', { level: 2, name: 'NBA games' })).toBeInTheDocument()
    expect(screen.getByText(/Play by play, with the score and the price\./)).toBeInTheDocument()
    expect(screen.queryByText(/replay/i)).toBeNull()
    expect(screen.getAllByRole('article')).toHaveLength(8)
  })

  it('renders nothing when there are no games', () => {
    const { container } = render(<ReplaysSection games={[]} />)

    expect(container).toBeEmptyDOMElement()
  })
})
