import { render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { describe, expect, it } from 'vitest'

import type { GeminiBoard } from './gemini-board-loader'
import { GeminiBoardSection } from './gemini-board-section'

const game = (id: string, league: string, title: string, extra: object = {}) => ({
  id,
  kind: 'game' as const,
  league,
  outcomes: [
    { cents: 73, label: 'Detroit' },
    { cents: 27, label: 'Toronto' },
  ],
  started: false,
  startsAt: '2026-11-21T00:30:00.000Z',
  teams: ['DET', 'TOR'],
  title,
  ...extra,
})

const question = (
  over: Partial<GeminiBoard['questions'][number]> = {}
): GeminiBoard['questions'][number] => ({
  category: 'Tech',
  closesAt: '2027-01-04T23:59:00.000Z',
  id: 'BESTAI26',
  kind: 'question',
  more: 2,
  outcomes: [
    { cents: 87, label: 'Gemini' },
    { cents: 11, label: 'Claude' },
    { cents: 5, label: 'ChatGPT' },
  ],
  title: 'Best AI at the end of 2026?',
  ...over,
})

const closesOn = new Intl.DateTimeFormat('en-US', { day: 'numeric', month: 'short' })

const board: GeminiBoard = {
  asOf: '2026-11-20T09:41:00.000Z',
  games: [
    game('NBA-1', 'NBA', 'Toronto vs Detroit', { tradeSlug: 'nba-h2h' }),
    game('NBA-2', 'NBA', 'Charlotte vs Atlanta'),
    game('NHL-1', 'NHL', 'Boston vs Montreal', { started: true }),
  ],
  questions: [],
  upDown: [
    {
      asset: 'BTC',
      closesAt: '2026-11-20T09:45:00.000Z',
      downCents: 50,
      id: 'BTC05M',
      kind: 'updown',
      minutes: 5,
      title: 'BTC Up or Down 5m',
      tradeSlug: 'btc-5m',
      upCents: 56,
    },
    {
      asset: 'ETH',
      closesAt: '2026-11-20T09:45:00.000Z',
      id: 'ETH15M',
      kind: 'updown',
      minutes: 15,
      title: 'ETH Up or Down 15m',
    },
  ],
}

function btcCard(): HTMLElement {
  return screen.getByRole('article', { name: 'BTC Up or Down 5m' })
}

describe('GeminiBoardSection', () => {
  it('lists every market with each side and its price, and says which ones Kyros trades', async () => {
    const { container } = render(<GeminiBoardSection board={board} />)

    expect(screen.getByRole('heading', { level: 2, name: 'Everything on Gemini' })).toBeVisible()
    const cards = screen.getAllByRole('article')
    expect(cards).toHaveLength(5)

    const btc = screen.getByRole('article', { name: 'BTC Up or Down 5m' })
    expect(within(btc).getByText('Up')).toBeVisible()
    expect(within(btc).getByText('56¢')).toBeVisible()
    expect(within(btc).getByText('50¢')).toBeVisible()
    expect(within(btc).getByRole('link', { name: /Trade on Kyros/ })).toHaveAttribute(
      'href',
      '/market/btc-5m'
    )

    const eth = screen.getByRole('article', { name: 'ETH Up or Down 15m' })
    expect(within(eth).queryByRole('link')).toBeNull()
    expect(within(eth).getByText('Shown from Gemini')).toBeVisible()
    // No price is better than an invented one.
    expect(within(eth).queryByText(/¢/)).toBeNull()

    const tor = screen.getByRole('article', { name: 'Toronto vs Detroit' })
    expect(within(tor).getByRole('link', { name: /Trade on Kyros/ })).toHaveAttribute(
      'href',
      '/market/nba-h2h'
    )
    expect(
      // The list gives a start time and no score, so that is all the card claims.
      within(screen.getByRole('article', { name: 'Boston vs Montreal' })).getByText('Started')
    ).toBeVisible()

    await expectNoA11yViolations(container)
  })

  it('filters by kind of market, with a count on each filter', async () => {
    const user = userEvent.setup()
    render(<GeminiBoardSection board={board} />)

    expect(screen.getByRole('button', { name: 'All 5' })).toHaveAttribute('aria-pressed', 'true')
    await user.click(screen.getByRole('button', { name: 'NBA 2' }))

    expect(screen.getAllByRole('article').map(card => card.getAttribute('aria-label'))).toEqual([
      'Toronto vs Detroit',
      'Charlotte vs Atlanta',
    ])

    await user.click(screen.getByRole('button', { name: 'Crypto 2' }))
    expect(screen.getAllByRole('article')).toHaveLength(2)
  })

  it('puts the up or down rounds first among the filters, then the leagues by size', () => {
    const games = [
      game('NFL-1', 'NFL', 'Game 1'),
      game('NFL-2', 'NFL', 'Game 2'),
      game('NFL-3', 'NFL', 'Game 3'),
      game('EPL-1', 'EPL', 'Game 4'),
    ]
    render(<GeminiBoardSection board={{ ...board, games }} />)

    const filters = within(screen.getByRole('group', { name: 'Kind of market' }))
      .getAllByRole('button')
      .map(button => button.textContent)
    expect(filters).toEqual(['All 6', 'Crypto 2', 'NFL 3', 'EPL 1'])
  })

  it('shows a long list a screenful at a time', async () => {
    const user = userEvent.setup()
    const many = Array.from({ length: 60 }, (_, index) =>
      game(`G-${index}`, 'NFL', `Game ${index}`)
    )
    render(<GeminiBoardSection board={{ ...board, games: many, upDown: [] }} />)

    expect(screen.getAllByRole('article')).toHaveLength(48)
    await user.click(screen.getByRole('button', { name: 'Show 12 more' }))
    expect(screen.getAllByRole('article')).toHaveLength(60)
    expect(screen.queryByRole('button', { name: /^Show/ })).toBeNull()
  })

  it('lights a price that has just changed, and leaves the others alone', () => {
    const { rerender } = render(<GeminiBoardSection board={board} />)
    // Nothing is lit on arrival: a page of flashing prices says nothing.
    expect(btcCard().querySelector('[data-ticked]')).toBeNull()

    const [first, ...rest] = board.upDown
    if (first === undefined) throw new Error('fixture has an up/down card')
    rerender(
      <GeminiBoardSection board={{ ...board, upDown: [{ ...first, upCents: 61 }, ...rest] }} />
    )

    expect(within(btcCard()).getByText('61¢')).toHaveAttribute('data-ticked')
    expect(within(btcCard()).getByText('50¢')).not.toHaveAttribute('data-ticked')
  })

  it('says when the list was read', () => {
    render(<GeminiBoardSection board={board} />)

    expect(screen.getByText(/Read from Gemini's public list at 09:41 UTC/)).toBeVisible()
  })

  it('renders a question card with its top outcomes, the rest counted, and its close date', async () => {
    const { container } = render(
      <GeminiBoardSection board={{ ...board, games: [], questions: [question()], upDown: [] }} />
    )

    const card = screen.getByRole('article', { name: 'Best AI at the end of 2026?' })
    expect(
      within(card).getByRole('heading', { level: 3, name: 'Best AI at the end of 2026?' })
    ).toBeVisible()
    const pairs: [string, string][] = [
      ['Gemini', '87¢'],
      ['Claude', '11¢'],
      ['ChatGPT', '5¢'],
    ]
    for (const [label, price] of pairs) {
      expect(within(card).getByText(label)).toBeVisible()
      expect(within(card).getByText(price)).toBeVisible()
    }
    expect(within(card).getByText('+2 more · Shown from Gemini')).toBeVisible()

    const when = card.querySelector('time')
    expect(when).toHaveAttribute('dateTime', '2027-01-04T23:59:00.000Z')
    expect(when).toHaveTextContent(
      `Closes ${closesOn.format(new Date('2027-01-04T23:59:00.000Z'))}`
    )

    await expectNoA11yViolations(container)
  })

  it('omits the "+N more" and the close date when there are none', () => {
    // A question with no expiry carries no closesAt key at all.
    const noClose: GeminiBoard['questions'][number] = {
      category: 'Tech',
      id: 'BESTAI26',
      kind: 'question',
      more: 0,
      outcomes: [{ cents: 87, label: 'Gemini' }],
      title: 'Best AI at the end of 2026?',
    }
    render(<GeminiBoardSection board={{ ...board, games: [], questions: [noClose], upDown: [] }} />)

    const card = screen.getByRole('article', { name: 'Best AI at the end of 2026?' })
    expect(within(card).getByText('Shown from Gemini')).toBeVisible()
    expect(within(card).queryByText(/more ·/)).toBeNull()
    expect(card.querySelector('time')).toBeNull()
  })

  it('files a question under its own category filter, with tradable cards first', async () => {
    const user = userEvent.setup()
    render(
      <GeminiBoardSection
        board={{
          ...board,
          games: [game('NBA-1', 'NBA', 'Toronto vs Detroit', { tradeSlug: 'nba-h2h' })],
          questions: [question()],
          upDown: [],
        }}
      />
    )

    // The tradable game comes before the (non-tradable) question.
    expect(screen.getAllByRole('article').map(card => card.getAttribute('aria-label'))).toEqual([
      'Toronto vs Detroit',
      'Best AI at the end of 2026?',
    ])

    await user.click(screen.getByRole('button', { name: 'Tech 1' }))
    expect(screen.getAllByRole('article').map(card => card.getAttribute('aria-label'))).toEqual([
      'Best AI at the end of 2026?',
    ])
  })

  it('files a non-up/down crypto question under the existing Crypto filter', () => {
    render(
      <GeminiBoardSection
        board={{
          ...board,
          games: [],
          questions: [question({ category: 'Crypto', id: 'BTC1H', title: 'BTC price range' })],
        }}
      />
    )

    // Two up/down cards plus the one crypto question.
    expect(screen.getByRole('button', { name: 'Crypto 3' })).toBeVisible()
  })
})
