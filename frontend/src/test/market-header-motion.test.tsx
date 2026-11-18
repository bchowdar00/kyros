// @vitest-environment-options {"pretendToBeVisual": true}
import { fireEvent, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it } from 'vitest'

import { MarketHeaderMotion } from '@/features/market-detail/market-header-motion'

function scrollDocument(offset: number) {
  document.documentElement.scrollTop = offset
  fireEvent.scroll(window)
}

afterEach(() => scrollDocument(0))

describe('MarketHeaderMotion', () => {
  it('hides the subtitle and its controls on scroll, then restores them at the top', async () => {
    Object.defineProperty(document, 'scrollingElement', {
      configurable: true,
      value: document.documentElement,
    })
    Object.defineProperty(document.documentElement, 'scrollHeight', {
      configurable: true,
      value: 2000,
    })
    const user = userEvent.setup()
    render(
      <MarketHeaderMotion
        actions={<button type="button">Buy UP</button>}
        actionSubtitle={<output aria-label="Potential payout">$100 to $200</output>}
        background={null}
        identity={<h1>BTC Up or Down</h1>}
        subtitle={<button type="button">Share market</button>}
      />
    )
    const share = screen.getByRole('button', { name: 'Share market' })
    const payout = screen.getByRole('status', { name: 'Potential payout' })
    expect(share).toBeVisible()
    expect(payout).toBeVisible()

    scrollDocument(120)
    await waitFor(() => expect(share).not.toBeVisible())
    expect(payout).not.toBeVisible()
    expect(screen.getByRole('heading', { name: 'BTC Up or Down' })).toBeVisible()
    await user.tab()
    expect(screen.getByRole('button', { name: 'Buy UP' })).toHaveFocus()

    scrollDocument(0)
    await waitFor(() => expect(share).toBeVisible())
    expect(payout).toBeVisible()
  })
})
