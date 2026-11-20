import { render, screen, within } from '@testing-library/react'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { describe, expect, it, vi } from 'vitest'

import type { RecordBoardView } from '@/features/records/record-board-loader'

import { HomeScreen } from './home-screen'

vi.mock('next/navigation', () => ({ useRouter: () => ({ refresh: vi.fn() }) }))

const EXPLORER = 'https://explorer.test'
const id = (letter: string) => `0x${letter.repeat(64)}`
const tx = (letter: string) => `0x${letter.repeat(64)}`

function row(letter: string, name: string | undefined, pnlCents: number, extra: object = {}) {
  return {
    budgetCents: 4000,
    deploymentId: id(letter),
    lastRoundId: 231,
    lastSealedAt: '2026-11-20T08:25:23.000000Z',
    lastTransactionHash: tx(letter),
    name,
    openTransactionHash: undefined,
    pnlCents,
    rounds: 12,
    series: 'btc-5m',
    wonRounds: 7,
    ...extra,
  }
}

function seal(roundId: number, name: string | undefined, pnlCents: number, minute: string) {
  return {
    at: `2026-11-20T08:${minute}:00.000000Z`,
    deploymentId: id('a'),
    name,
    pnlCents,
    roundId,
    series: name === undefined ? undefined : 'btc-5m',
    transactionHash: `0x${String(roundId).padStart(64, '0')}`,
    won: pnlCents > 0,
  }
}

const records: Extract<RecordBoardView, { status: 'ok' }> = {
  asOf: '2026-11-20T08:30:00.000Z',
  contract: '0xc910cfe6b537feee16b60a9964c55fc66580586a',
  recent: [
    seal(231, 'Inside the Spread', 120, '25'),
    seal(230, undefined, -996, '20'),
    seal(229, 'Inside the Spread', 0, '15'),
    seal(228, 'Inside the Spread', 5, '10'),
    seal(227, 'Inside the Spread', 5, '09'),
    seal(226, 'Inside the Spread', 5, '08'),
    seal(225, 'Inside the Spread', 5, '07'),
  ],
  rows: [
    row('a', 'Inside the Spread', 1250, { copy: { inverse: false, title: 'Inside the Spread' } }),
    row('b', 'My Own Idea', 300),
    row('c', undefined, 0, { series: undefined }),
    row('d', 'Fourth Place', -40),
    row('e', 'Fifth Place', -50),
    row('f', 'Sixth Place', -60),
  ],
  status: 'ok',
  totals: { rounds: 48, strategies: 6, wonRounds: 28 },
  truncated: false,
}

function renderHome(withRecords = true) {
  return render(
    <HomeScreen cards={[]} explorerUrl={EXPLORER} records={withRecords ? records : undefined} />
  )
}

describe('the home page as a product front', () => {
  it('opens with what Kyros does, in words that are true, and where to start', async () => {
    const { container } = renderHome()

    expect(
      screen.getByRole('heading', { level: 1, name: 'Put a strategy to work on live markets' })
    ).toBeVisible()
    expect(screen.getByText(/Pretend money, live Gemini prices/)).toBeVisible()
    expect(screen.getByRole('link', { name: 'Pick a market' })).toHaveAttribute('href', '#markets')
    expect(screen.getByRole('link', { name: 'See the leaderboard' })).toHaveAttribute(
      'href',
      '/leaderboard'
    )
    expect(screen.getByRole('heading', { level: 2, name: 'Markets' })).toBeVisible()

    await expectNoA11yViolations(container)
  })

  it('says how it works in three steps, in words that are true', () => {
    renderHome()

    const steps = within(screen.getByRole('region', { name: 'How it works' }))
    expect(steps.getAllByRole('heading', { level: 3 }).map(step => step.textContent)).toEqual([
      'Say what you want',
      'Checked before it trades',
      'Sealed where anyone can look',
    ])
    expect(
      steps.getByText(/An LLM writes it\. A compiler checks it before any money moves\./)
    ).toBeVisible()
    expect(
      steps.getByText(/It runs in its own account, holding only the budget you give it\./)
    ).toBeVisible()
  })

  it('counts what is sealed on Arc, from the record and nowhere else', () => {
    renderHome()

    const stats = within(screen.getByRole('region', { name: 'Sealed on Arc so far' }))
    const valueOf = (label: string) => {
      const value = stats.getByText(label).nextElementSibling
      if (!(value instanceof HTMLElement)) throw new Error(`no value beside ${label}`)
      return within(value)
    }
    expect(valueOf('Strategies on the record').getByText('6')).toBeInTheDocument()
    expect(valueOf('Rounds sealed').getByText('48')).toBeInTheDocument()
    expect(valueOf('Rounds won').getByText('28')).toBeInTheDocument()
  })

  it('shows the three best strategies, and lets a public one be copied', () => {
    renderHome()

    const top = within(screen.getByRole('region', { name: 'Top strategies' }))
    const items = top.getAllByRole('listitem')
    expect(items.map(item => within(item).getByRole('heading').textContent)).toEqual([
      'Inside the Spread',
      'My Own Idea',
      `Strategy 0xcccc…cccc`,
    ])
    const [first, second] = items
    if (first === undefined || second === undefined) throw new Error('expected three strategies')
    expect(within(first).getByText('+$12.50')).toBeVisible()
    expect(within(first).getByText(/58% of 12 rounds won/)).toBeVisible()

    const copy = within(first).getByRole('link', { name: 'Copy: Inside the Spread' })
    const target = new URL(copy.getAttribute('href') ?? '', 'https://kyros.test')
    expect(target.pathname).toBe('/market/btc-5m')
    expect(target.searchParams.get('copy')).toBe('Inside the Spread')
    // A strategy its writer alone holds cannot be copied.
    expect(within(second).queryByRole('link', { name: /^Copy/ })).toBeNull()
    expect(top.getByRole('link', { name: 'Full leaderboard' })).toHaveAttribute(
      'href',
      '/leaderboard'
    )
  })

  it('lists the five newest sealed rounds, each with its result and its transaction', () => {
    renderHome()

    const feed = within(screen.getByRole('region', { name: 'Just sealed' }))
    const items = feed.getAllByRole('listitem')
    expect(items).toHaveLength(5)

    const [newest, unnamed, flat] = items
    if (newest === undefined || unnamed === undefined || flat === undefined) {
      throw new Error('expected five rounds')
    }
    expect(newest).toHaveTextContent('Inside the Spread')
    expect(newest).toHaveTextContent('BTC 5m · round 231')
    expect(newest).toHaveTextContent('+$1.20')
    expect(newest).toHaveTextContent('Nov 20, 08:25 UTC')
    expect(
      within(newest).getByRole('link', { name: 'View on Arc: round 231 of Inside the Spread' })
    ).toHaveAttribute('href', `${EXPLORER}/tx/0x${'231'.padStart(64, '0')}`)

    expect(unnamed).toHaveTextContent('Strategy 0xaaaa…aaaa')
    expect(unnamed).toHaveTextContent('-$9.96')
    // A round sealed at nothing is neither a gain nor a loss.
    expect(within(flat).getByText('$0.00')).not.toHaveClass('text-positive')
  })

  it('leaves the record sections out when Arc could not be read, and keeps the rest', () => {
    renderHome(false)

    expect(screen.getByRole('heading', { level: 1 })).toBeVisible()
    expect(screen.queryByRole('region', { name: 'Top strategies' })).toBeNull()
    expect(screen.queryByRole('region', { name: 'Just sealed' })).toBeNull()
    expect(screen.queryByRole('region', { name: 'Sealed on Arc so far' })).toBeNull()
  })

  it('shows the Gemini board when all it has is questions, and not when it is empty', () => {
    const question = {
      category: 'Politics',
      id: 'SAVEACT',
      kind: 'question' as const,
      more: 0,
      outcomes: [{ cents: 31, label: 'Yes' }],
      title: 'Will the SAVE Act become law?',
    }
    const board = { asOf: '2026-11-20T11:00:00.000Z', games: [], questions: [question], upDown: [] }

    const { rerender } = render(<HomeScreen board={board} cards={[]} />)
    expect(screen.getByText('Will the SAVE Act become law?')).toBeVisible()

    rerender(<HomeScreen board={{ ...board, questions: [] }} cards={[]} />)
    expect(screen.queryByText('Will the SAVE Act become law?')).toBeNull()
    expect(document.querySelector('#gemini-board-title')).toBeNull()
  })

  it('adds the games other venues price, leaving out any league Gemini lists itself', () => {
    const game = {
      id: 'KXSERIEAGAME-X',
      outcomes: [
        { label: 'Como', percent: 75.5 },
        { label: 'Cagliari', percent: 9.5 },
      ],
      title: 'Como vs Cagliari',
      volume24h: 10,
      when: 'Nov 21',
    }
    const venues = {
      leagues: [
        { games: [game], label: 'Serie A' },
        { games: [{ ...game, id: 'KXUFCFIGHT-X', title: 'Nurgozhay vs Lorenz' }], label: 'UFC' },
      ],
    }
    const listed = {
      category: 'Serie A',
      id: 'Q',
      kind: 'question' as const,
      more: 0,
      outcomes: [],
      title: 'Who wins Serie A?',
    }
    const nba = {
      id: 'NBA-X',
      kind: 'game' as const,
      league: 'NBA',
      outcomes: [],
      started: false,
      startsAt: '2026-11-21T00:30:00.000Z',
      teams: ['CHA', 'ATL'],
      title: 'Charlotte at Atlanta',
    }
    const board = {
      asOf: '2026-11-20T11:00:00.000Z',
      games: [nba],
      questions: [listed],
      upDown: [],
    }

    const { rerender } = render(<HomeScreen board={board} cards={[]} venues={venues} />)

    const section = screen.getByRole('region', { name: 'More games, priced on Kalshi' })
    expect(within(section).getByRole('heading', { level: 3, name: 'UFC' })).toBeVisible()
    expect(within(section).queryByRole('heading', { level: 3, name: 'Serie A' })).toBeNull()

    // Without Gemini's own list of games there is nothing to check "not on Gemini" against.
    rerender(<HomeScreen board={{ ...board, games: [] }} cards={[]} venues={venues} />)
    expect(screen.queryByRole('region', { name: 'More games, priced on Kalshi' })).toBeNull()
    rerender(<HomeScreen cards={[]} venues={venues} />)
    expect(screen.queryByRole('region', { name: 'More games, priced on Kalshi' })).toBeNull()
  })

  it('shows the game replays under the markets, and no such band when there are none', () => {
    const entry = {
      away: { code: 'GS', name: 'Golden State Warriors' },
      date: '2026-11-19',
      durationMs: 60_000,
      final: { away: 101, home: 110 },
      home: { code: 'MEM', name: 'Memphis Grizzlies' },
      id: '401910019',
    }
    vi.stubGlobal(
      'fetch',
      vi.fn(() => Promise.reject(new Error('offline')))
    )

    const { rerender } = render(<HomeScreen cards={[]} replays={[entry]} />)
    const band = screen.getByRole('region', { name: 'Game replays' })
    expect(within(band).getByText(/Replay · Nov 19/)).toBeVisible()
    expect(within(band).getByRole('link')).toHaveAttribute('href', '/replay/401910019')

    rerender(<HomeScreen cards={[]} />)
    expect(screen.queryByRole('region', { name: 'Game replays' })).toBeNull()
    vi.unstubAllGlobals()
  })
})
