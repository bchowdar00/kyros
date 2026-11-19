import { cleanup, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it } from 'vitest'

import { OrderBookPanel } from './order-book-panel'
import type { OrderBookData } from './types'

const book: OrderBookData = {
  title: 'Order book',
  actionLabel: 'Center order book',
  columnLabels: ['Price (YES)', 'Lots', 'Total'],
  asks: [],
  bids: [],
  spreadLabel: 'Spread',
  spreadTicks: null,
}
const gemini = {
  env: 'production' as const,
  series: 'BTC05M',
  phase: 'open' as const,
  book: 'live' as const,
}
const level = { priceTicks: 500, qty: '10', totalMicros: '5000000', depthPercentage: 100 }

afterEach(cleanup)

describe('order book panel', () => {
  it('says each empty side is empty on Gemini while the feed is live and Gemini backs it', () => {
    render(<OrderBookPanel {...book} feedLive geminiBook={{ status: 'polled', gemini }} />)
    expect(screen.getByText('No offers on Gemini right now')).toBeInTheDocument()
    expect(screen.getByText('No bids on Gemini right now')).toBeInTheDocument()
  })

  it('does not name Gemini for an empty side when no Gemini block is known', () => {
    render(
      <OrderBookPanel {...book} feedLive geminiBook={{ status: 'polled', gemini: undefined }} />
    )
    expect(screen.getByText('No offers right now')).toBeInTheDocument()
    expect(screen.getByText('No bids right now')).toBeInTheDocument()
    expect(screen.queryByText(/Gemini/)).not.toBeInTheDocument()
  })

  it('names only the side that is empty', () => {
    render(
      <OrderBookPanel {...book} asks={[level]} feedLive geminiBook={{ status: 'polled', gemini }} />
    )
    expect(screen.queryByText('No offers on Gemini right now')).not.toBeInTheDocument()
    expect(screen.getByText('No bids on Gemini right now')).toBeInTheDocument()
  })

  it('claims nothing about an empty book it cannot see', () => {
    render(<OrderBookPanel {...book} feedLive={false} />)
    expect(screen.queryByText(/^No offers/)).not.toBeInTheDocument()
    expect(screen.queryByText(/^No bids/)).not.toBeInTheDocument()
  })

  it('shows where the book comes from', () => {
    render(<OrderBookPanel {...book} geminiBook={{ status: 'polled', gemini }} />)
    expect(screen.getByText('Book from Gemini')).toBeInTheDocument()
    expect(screen.getByText('Live')).toBeInTheDocument()
  })
})
