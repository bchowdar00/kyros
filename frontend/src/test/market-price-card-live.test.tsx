import { act, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { FakeWebSocket } from '@/features/exchange-streams/fake-web-socket.fixture'
import { getMarketDetailFixture } from '@/features/market-detail/market-detail.fixture'
import { MarketPriceCard } from '@/features/market-detail/market-price-card'

vi.mock('@/env', () => ({ env: { NEXT_PUBLIC_API_URL: 'http://localhost:8080' } }))

const market = getMarketDetailFixture('btc-above-118k-0903')

if (!market?.priceChart) {
  throw new Error('The bitcoin fixture is expected to carry a price chart.')
}

const priceChart = market.priceChart
const socketFor = (path: string) =>
  FakeWebSocket.instances.find(instance => instance.url.includes(path))

describe('MarketPriceCard on the live exchange', () => {
  beforeEach(() => {
    vi.stubGlobal('WebSocket', FakeWebSocket)
    // jsdom reports a zero-size container, and a chart with no size draws nothing.
    vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockReturnValue({
      bottom: 493,
      height: 493,
      left: 0,
      right: 1110,
      toJSON: () => ({}),
      top: 0,
      width: 1110,
      x: 0,
      y: 0,
    })
  })

  afterEach(() => {
    FakeWebSocket.reset()
    vi.unstubAllGlobals()
    vi.restoreAllMocks()
  })

  // Streams are shared per series and outlive a test, so each test gets its own series.
  it("plots the book's midpoint in the probability view, not a mapping of the price", () => {
    const now = Date.now()
    const { container } = render(<MarketPriceCard {...priceChart} priceSymbol="btc-5m" />)
    const price = socketFor('/ws/price')
    const book = socketFor('/ws/book')
    expect(price?.url).toBe('ws://localhost:8080/ws/price?series=btc-5m')
    expect(book?.url).toBe('ws://localhost:8080/ws/book?series=btc-5m')

    act(() => {
      // BTC well above the strike: the old distance mapping would read about 92%.
      price?.emit({
        type: 'snapshot',
        series: 'btc-5m',
        duration_secs: 300,
        market_id: 7,
        strike: 118_000,
        opens_at_ms: now - 60_000,
        closes_at_ms: now + 60_000,
        ticks: [
          { ts_ms: now - 60_000, price: 118_000 },
          { ts_ms: now - 1_000, price: 118_120 },
        ],
      })
      book?.emit({
        type: 'snapshot',
        series: 'btc-5m',
        market_id: 7,
        mid: 450,
        mid_history: [
          { ts_ms: now - 50_000, mid: 500 },
          { ts_ms: now - 2_000, mid: 450 },
        ],
        bids: [{ price: 400, qty: 5 }],
        asks: [{ price: 500, qty: 5 }],
      })
    })
    fireEvent.click(screen.getByRole('radio', { name: 'Market probability chart' }))

    const label = (id: string) =>
      container.querySelector(`[data-chart-endpoint-label="${id}"] text`)?.textContent
    expect(label('yes')).toBe('45%')
    expect(label('no')).toBe('55%')
  })

  it('draws no probability line until the book prints a midpoint', () => {
    const { container } = render(<MarketPriceCard {...priceChart} priceSymbol="btc-15m" />)
    fireEvent.click(screen.getByRole('radio', { name: 'Market probability chart' }))

    expect(container.querySelector('[data-chart-endpoint-label="yes"]')).toBeNull()
  })
})
