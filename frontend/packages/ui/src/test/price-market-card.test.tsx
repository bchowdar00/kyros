import { act, render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { afterEach, describe, expect, it, vi } from 'vitest'

import { PriceMarketCard } from '../charts/price-market-card'

/** jsdom lays nothing out; the chart only draws once its container reports a size. */
function giveChartsASize() {
  const original = HTMLElement.prototype.getBoundingClientRect
  vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(function (
    this: HTMLElement
  ) {
    return this.tagName === 'FIGURE' ? new DOMRect(0, 0, 1110, 292) : original.call(this)
  })
}

describe('PriceMarketCard', () => {
  afterEach(() => {
    vi.restoreAllMocks()
  })

  it('plots the supplied market probability instead of mapping the price distance', async () => {
    giveChartsASize()
    const user = userEvent.setup()
    const start = Date.now() - 60_000
    // Price sits well above the strike: the distance mapping would say about 92%.
    render(
      <PriceMarketCard
        asset={{ color: 'var(--asset-btc)', symbol: 'BTC' }}
        feed={{ state: 'live' }}
        points={[
          { time: start, value: 118_000 },
          { time: start + 60_000, value: 118_120 },
        ]}
        priceBand={300}
        probabilityPoints={[
          { time: start, value: 50 },
          { time: start + 60_000, value: 45 },
        ]}
        targetLabel="Price to beat"
        targetPrice={118_000}
        timeZone="UTC"
        windows={[{ candleInterval: 5_000, durationMs: 60_000 }]}
      />
    )
    await act(async () => {
      await user.click(screen.getByRole('radio', { name: 'Market probability chart' }))
    })
    const figure = screen.getByRole('figure', { name: 'BTC probability chart' })
    const label = (id: string) =>
      figure.querySelector(`[data-chart-endpoint-label="${id}"] text`)?.textContent
    expect(label('yes')).toBe('45%')
    expect(label('no')).toBe('55%')
  })

  it('renders the chart from an application-owned live feed without market summary UI', async () => {
    // A live window runs on the wall clock, so the samples hang off it.
    const start = Date.now() - 60_000
    const { container } = render(
      <PriceMarketCard
        asset={{ color: 'var(--asset-btc)', symbol: 'BTC' }}
        feed={{ state: 'live' }}
        points={[
          { time: start, value: 118_000 },
          { time: start + 60_000, value: 118_120 },
        ]}
        priceBand={300}
        targetLabel="Price to beat"
        targetPrice={118_000}
        timeZone="America/Los_Angeles"
        windows={[{ candleInterval: 5_000, durationMs: 60_000 }]}
      />
    )

    expect(screen.getByRole('figure', { name: 'BTC price chart' })).toBeInTheDocument()
    expect(screen.queryByText('Current')).not.toBeInTheDocument()
    expect(screen.queryByText('Time Left')).not.toBeInTheDocument()
    expect(screen.queryByText('Simulated data')).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: 'Stop live feed' })).not.toBeInTheDocument()
    expect(screen.queryByRole('radiogroup', { name: 'BTC chart window' })).not.toBeInTheDocument()
    await expectNoA11yViolations(container)
  })
})
