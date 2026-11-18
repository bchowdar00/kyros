import { act, fireEvent, render, screen } from '@testing-library/react'
import { AppRouterContext } from 'next/dist/shared/lib/app-router-context.shared-runtime'
import { http, HttpResponse } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest'

import { MarketDetailScreen } from '@/features/market-detail/market-detail-screen'
import { MARKET_DETAIL_FIXTURES } from '@/features/market-detail/market-detail.fixture'

const server = setupServer(
  http.get('*/api/series/1/active-market', () => HttpResponse.json({ eventId: 2 })),
  http.get('*/api/chats', () => new HttpResponse(null, { status: 503 }))
)
const router = {
  back: vi.fn(),
  bfcacheId: 'bitcoin',
  forward: vi.fn(),
  prefetch: vi.fn(),
  push: vi.fn(),
  refresh: vi.fn(),
  replace: vi.fn(),
}

beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => vi.restoreAllMocks())
afterAll(() => server.close())

describe('market chart rollover', () => {
  it('keeps keyboard selection and the chosen chart view when the next event arrives', () => {
    vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockReturnValue({
      bottom: 493,
      height: 493,
      left: 0,
      right: 1110,
      top: 0,
      width: 1110,
      x: 0,
      y: 0,
      toJSON: () => ({}),
    })
    const market = MARKET_DETAIL_FIXTURES['btc-above-118k-0903']
    const screenFor = (eventId: number) => (
      <AppRouterContext.Provider value={router}>
        <MarketDetailScreen market={{ ...market, liveEvent: { eventId, seriesId: 1 } }} />
      </AppRouterContext.Provider>
    )
    const { rerender } = render(screenFor(1))
    fireEvent.click(screen.getByRole('radio', { name: 'Market probability chart' }))
    const slider = screen.getByRole('slider', { name: /^BTC probability chart/ })
    act(() => slider.focus())
    fireEvent.keyDown(slider, { key: 'ArrowLeft' })
    const selection = slider.getAttribute('aria-valuetext')
    expect(selection).toBeTruthy()

    rerender(screenFor(2))

    expect(screen.getByRole('radio', { name: 'Market probability chart' })).toBeChecked()
    expect(screen.getByRole('slider', { name: /^BTC probability chart/ })).toHaveFocus()
    expect(screen.getByRole('slider', { name: /^BTC probability chart/ })).toHaveAttribute(
      'aria-valuetext',
      selection
    )
  })
})
