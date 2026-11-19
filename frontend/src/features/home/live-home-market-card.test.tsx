import { act, render, screen } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { FakeWebSocket } from '@/features/exchange-streams/fake-web-socket.fixture'
import { toHomeMarketCard } from '@/features/market-detail/event-detail.mapper'
import { toEventDetail, toSeriesRecord } from '@/features/market-detail/exchange-catalog.mapper'

import { LiveHomeMarketCard } from './live-home-market-card'

vi.mock('@/env', () => ({ env: { NEXT_PUBLIC_API_URL: 'http://localhost:8080' } }))

// Streams are shared per series and outlive a test, so this file has its own series slug.
const gameInfo = {
  series: 'nba-home-card',
  duration_secs: 300,
  underlying: 'nba',
  outcomes: ['home', 'away'],
  current_market_id: 3,
  opens_at_ms: 1_795_060_472_000,
  closes_at_ms: 1_795_069_800_000,
  reference: { home_team: 'DEN', away_team: 'BOS', pregame_home_prob: 0.5, game_seq: 1 },
}
const gameDetail = toEventDetail(gameInfo, 100_003)
if (gameDetail === undefined) throw new Error('the game fixture must build an event')
const gameCard = toHomeMarketCard(toSeriesRecord(gameInfo, 100_003), gameDetail)
if (gameCard === undefined) throw new Error('the game fixture must build a card')

describe('a game card on the markets page', () => {
  beforeEach(() => {
    vi.stubGlobal('WebSocket', FakeWebSocket)
  })

  afterEach(() => {
    FakeWebSocket.reset()
    vi.unstubAllGlobals()
  })

  it("draws the home team's price from the book once the book has printed one", () => {
    render(<LiveHomeMarketCard card={gameCard} />)
    const book = FakeWebSocket.instances.find(instance => instance.url.includes('/ws/book'))
    expect(book?.url).toBe('ws://localhost:8080/ws/book?series=nba-home-card')
    // Nothing is drawn before the book says anything: no placeholder line.
    expect(screen.getByText('Chart unavailable')).toBeVisible()

    act(() => {
      book?.emit({
        type: 'snapshot',
        series: 'nba-home-card',
        market_id: 3,
        mid: 300,
        mid_history: [
          { ts_ms: 1_795_060_480_000, mid: 520 },
          { ts_ms: 1_795_060_500_000, mid: 300 },
        ],
        bids: [{ price: 290, qty: 5 }],
        asks: [{ price: 310, qty: 5 }],
      })
    })

    expect(screen.queryByText('Chart unavailable')).toBeNull()
    expect(screen.getByRole('figure', { name: 'BOS at DEN trend' })).toBeVisible()
  })
})
