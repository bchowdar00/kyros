import { fireEvent, render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it, vi } from 'vitest'

import { MARKET_DETAIL_FIXTURES } from '@/features/market-detail/market-detail.fixture'
import { OrderBookPanel } from '@/features/market-detail/order-book-panel'

const orderBook = MARKET_DETAIL_FIXTURES['btc-above-118k-0903'].orderBook

describe('OrderBookPanel', () => {
  it('follows arriving depth until manually scrolled and resumes with the center button', () => {
    const { rerender } = render(<OrderBookPanel {...orderBook} asks={[]} bids={[]} />)
    const region = screen.getByRole('rowgroup', { name: 'Order book levels' })
    const spread = screen.getByRole('rowheader', { name: orderBook.spreadLabel }).parentElement
    if (!spread) throw new Error('Missing spread row')
    Object.defineProperty(region, 'clientHeight', { configurable: true, value: 385 })
    Object.defineProperty(spread, 'offsetHeight', { configurable: true, value: 28 })
    vi.spyOn(region, 'getBoundingClientRect').mockImplementation(
      () => new DOMRect(0, 100, 600, 385)
    )
    let spreadOffset = 400
    vi.spyOn(spread, 'getBoundingClientRect').mockImplementation(
      () => new DOMRect(0, 100 + spreadOffset - region.scrollTop, 600, 28)
    )
    rerender(<OrderBookPanel {...orderBook} />)
    expect(region.scrollTop).toBe(221.5)

    fireEvent.wheel(region, { deltaY: 100 })
    region.scrollTop = 300
    spreadOffset = 432
    rerender(<OrderBookPanel {...orderBook} asks={orderBook.asks.slice(1)} />)
    expect(region.scrollTop).toBe(300)

    fireEvent.click(screen.getByRole('button', { name: orderBook.actionLabel }))
    expect(region.scrollTop).toBe(253.5)
    spreadOffset = 400
    rerender(<OrderBookPanel {...orderBook} />)
    expect(region.scrollTop).toBe(221.5)
  })

  it('renders fixture-backed asks, bids, headings, and spread as a semantic table', () => {
    const { container } = render(<OrderBookPanel {...orderBook} />)

    const panel = screen.getByRole('region', { name: orderBook.title })
    const table = within(panel).getByRole('table', { name: orderBook.title })
    const columnHeaders = within(table).getAllByRole('columnheader')

    expect(columnHeaders.map(header => header.textContent)).toEqual(orderBook.columnLabels)
    expect(container.querySelectorAll('[data-order-book-side="away"]')).toHaveLength(
      orderBook.asks.length
    )
    expect(container.querySelectorAll('[data-order-book-side="home"]')).toHaveLength(
      orderBook.bids.length
    )
    expect(within(table).getByText('58¢')).toBeInTheDocument()
    expect(within(table).getByText('240.11')).toBeInTheDocument()
    expect(within(table).getByText('$1,633.75')).toBeInTheDocument()
    expect(
      within(table).getByRole('rowheader', { name: orderBook.spreadLabel })
    ).toBeInTheDocument()
    expect(within(table).getByText('1¢')).toBeInTheDocument()
    expect(within(table).getByText('1.60%')).toBeInTheDocument()
    expect(container.querySelector('[data-order-book-scroll]')).toBeInTheDocument()
  })

  it('sizes depth bars proportionally and exposes a spread-centering control', async () => {
    const user = userEvent.setup()
    const { container } = render(<OrderBookPanel {...orderBook} />)
    const depthBars = [...container.querySelectorAll<HTMLElement>('[data-order-book-depth]')]

    expect(depthBars).toHaveLength(orderBook.asks.length + orderBook.bids.length)
    expect(Number.parseFloat(depthBars[0]?.style.width ?? '0')).toBeGreaterThan(
      Number.parseFloat(depthBars[1]?.style.width ?? '0')
    )

    const centerButton = screen.getByRole('button', { name: orderBook.actionLabel })
    expect(centerButton).toBeInTheDocument()
    await user.click(centerButton)
  })
})
