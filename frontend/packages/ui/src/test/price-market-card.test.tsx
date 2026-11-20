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

  it('marks an order on the price line where the price was, and on the market line at the price paid', async () => {
    giveChartsASize()
    const user = userEvent.setup()
    const start = Date.now() - 40_000
    render(
      <PriceMarketCard
        asset={{ color: 'var(--asset-btc)', symbol: 'BTC' }}
        feed={{ state: 'live' }}
        points={[
          { time: start, value: 118_000 },
          { time: start + 40_000, value: 118_120 },
        ]}
        priceBand={300}
        probabilityPoints={[
          { time: start, value: 50 },
          { time: start + 40_000, value: 45 },
        ]}
        targetLabel="Price to beat"
        targetPrice={118_000}
        timeZone="UTC"
        tradeMarks={[
          {
            id: '7',
            label: 'Bought 10 Up at 52¢',
            percent: 52,
            side: 'first',
            time: start + 20_000,
          },
        ]}
        windows={[{ candleInterval: 5_000, durationMs: 60_000 }]}
      />
    )

    const onPrice = screen.getByRole('img', { name: 'Bought 10 Up at 52¢' })
    expect(onPrice).toHaveAttribute('data-trade-mark', 'first')

    await act(async () => {
      await user.click(screen.getByRole('radio', { name: 'Market probability chart' }))
    })
    expect(screen.getByRole('img', { name: 'Bought 10 Up at 52¢' })).toBeInTheDocument()
  })

  it("draws another venue's price on the market line only, never on the asset's price", async () => {
    giveChartsASize()
    const user = userEvent.setup()
    const start = Date.now() - 40_000
    const { container } = render(
      <PriceMarketCard
        asset={{ color: 'var(--asset-btc)', symbol: 'BTC' }}
        feed={{ state: 'live' }}
        points={[
          { time: start, value: 118_000 },
          { time: start + 40_000, value: 118_120 },
        ]}
        priceBand={300}
        probabilityPoints={[
          { time: start, value: 50 },
          { time: start + 40_000, value: 45 },
        ]}
        targetLabel="Price to beat"
        targetPrice={118_000}
        timeZone="UTC"
        venueLines={[
          {
            id: 'polymarket',
            label: 'Polymarket',
            points: [
              { time: start, value: 51 },
              { time: start + 40_000, value: 47 },
            ],
          },
        ]}
        windows={[{ candleInterval: 5_000, durationMs: 60_000 }]}
      />
    )

    expect(container.querySelector('[data-venue-line]')).toBeNull()

    await act(async () => {
      await user.click(screen.getByRole('radio', { name: 'Market probability chart' }))
    })
    expect(container.querySelector('[data-venue-line="polymarket"] text')?.textContent).toBe(
      'Polymarket 47%'
    )
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
