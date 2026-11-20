import { act, render, screen } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { FakeWebSocket } from '@/features/exchange-streams/fake-web-socket.fixture'
import { toHomeMarketCard } from '@/features/market-detail/event-detail.mapper'
import { toEventDetail, toSeriesRecord } from '@/features/market-detail/exchange-catalog.mapper'

import { withCardPast } from './card-past'
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
    // The book quotes the home team; the other side is what is left of a dollar.
    expect(screen.getByText('DEN 30¢')).toBeVisible()
    expect(screen.getByText('BOS 70¢')).toBeVisible()
  })

  it('keeps its line when the book has only printed one price, as before tipoff', () => {
    const opened = withCardPast({ ...gameCard, slug: 'nba-home-card-quiet' }, [
      { time: 1_795_060_000_000, value: 0.7 },
      { time: 1_795_060_060_000, value: 0.72 },
    ])
    render(<LiveHomeMarketCard card={opened} />)
    const book = FakeWebSocket.instances.find(instance => instance.url.includes('/ws/book'))

    act(() => {
      book?.emit({
        type: 'snapshot',
        series: 'nba-home-card-quiet',
        market_id: 3,
        mid: 705,
        mid_history: [{ ts_ms: 1_795_060_480_000, mid: 705 }],
        bids: [{ price: 700, qty: 5 }],
        asks: [{ price: 710, qty: 5 }],
      })
    })

    expect(screen.queryByText('Chart unavailable')).toBeNull()
    expect(screen.getByRole('figure', { name: 'BOS at DEN trend' })).toBeVisible()
    expect(screen.getByText('DEN 71¢')).toBeVisible()
  })
})

const strikeInfo = {
  series: 'btc-home-card',
  duration_secs: 300,
  underlying: 'btc',
  outcomes: ['up', 'down'],
  current_market_id: 7,
  opens_at_ms: 1_795_060_472_000,
  closes_at_ms: 1_795_060_772_000,
  reference: { strike: 118_000 },
}
const strikeDetail = toEventDetail(strikeInfo, 100_007)
if (strikeDetail === undefined) throw new Error('the strike fixture must build an event')
const strikeCard = toHomeMarketCard(toSeriesRecord(strikeInfo, 100_007), strikeDetail)
if (strikeCard === undefined) throw new Error('the strike fixture must build a card')

describe('a Bitcoin card on the markets page', () => {
  beforeEach(() => {
    vi.stubGlobal('WebSocket', FakeWebSocket)
  })

  afterEach(() => {
    FakeWebSocket.reset()
    vi.unstubAllGlobals()
  })

  it('shows what each side costs and how long the round has left', () => {
    render(<LiveHomeMarketCard card={strikeCard} />)
    const price = FakeWebSocket.instances.find(instance => instance.url.includes('/ws/price'))
    const book = FakeWebSocket.instances.find(instance => instance.url.includes('/ws/book'))
    expect(book?.url).toBe('ws://localhost:8080/ws/book?series=btc-home-card')
    const now = Date.now()

    act(() => {
      price?.emit({
        type: 'snapshot',
        series: 'btc-home-card',
        duration_secs: 300,
        market_id: 7,
        strike: 118_000,
        opens_at_ms: now - 60_000,
        closes_at_ms: now + 125_000,
        ticks: [
          { ts_ms: now - 60_000, price: 118_000 },
          { ts_ms: now - 1_000, price: 118_120 },
        ],
      })
      book?.emit({
        type: 'snapshot',
        series: 'btc-home-card',
        market_id: 7,
        mid: 372,
        mid_history: [{ ts_ms: now - 2_000, mid: 372 }],
        bids: [{ price: 360, qty: 5 }],
        asks: [{ price: 384, qty: 5 }],
      })
    })

    expect(screen.getByText('Up 37¢')).toBeVisible()
    expect(screen.getByText('Down 63¢')).toBeVisible()
    expect(screen.getByText(/^02:0[45] left$/)).toBeVisible()
  })

  it('never prices a side at a whole dollar or at nothing', () => {
    render(<LiveHomeMarketCard card={{ ...strikeCard, slug: 'btc-home-card-decided' }} />)
    const price = FakeWebSocket.instances.find(instance => instance.url.includes('/ws/price'))
    const book = FakeWebSocket.instances.find(instance => instance.url.includes('/ws/book'))
    const now = Date.now()

    act(() => {
      price?.emit({
        type: 'snapshot',
        series: 'btc-home-card-decided',
        duration_secs: 300,
        market_id: 7,
        strike: 118_000,
        opens_at_ms: now - 290_000,
        closes_at_ms: now + 10_000,
        ticks: [{ ts_ms: now - 1_000, price: 118_900 }],
      })
      book?.emit({
        type: 'snapshot',
        series: 'btc-home-card-decided',
        market_id: 7,
        mid: 996,
        mid_history: [{ ts_ms: now - 2_000, mid: 996 }],
        bids: [{ price: 995, qty: 5 }],
        asks: [{ price: 997, qty: 5 }],
      })
    })

    expect(screen.getByText('Up 99¢')).toBeVisible()
    expect(screen.getByText('Down 1¢')).toBeVisible()
  })

  it('shows no price while the book has none', () => {
    // A slug of its own: the stream above is shared by series and still holds its book.
    render(<LiveHomeMarketCard card={{ ...strikeCard, slug: 'btc-home-card-quiet' }} />)

    expect(screen.getByText('Up')).toBeVisible()
    expect(screen.getByText('Down')).toBeVisible()
    expect(screen.queryByText(/¢/)).toBeNull()
  })
})
