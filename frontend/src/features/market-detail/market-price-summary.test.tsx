import { render, screen } from '@testing-library/react'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { describe, expect, it } from 'vitest'

import { MarketPriceSummary } from './market-price-summary'

const base = {
  price: 118_136.42,
  targetLabel: 'Price to beat',
  targetPrice: 118_000,
  timeLeft: '00:12',
}

describe('MarketPriceSummary', () => {
  it('renders the target, current price, change and remaining time in four equal stats', async () => {
    const { container } = render(<MarketPriceSummary {...base} feed={{ state: 'live' }} />)

    expect(screen.getByText('$118,000.00')).toBeInTheDocument()
    expect(screen.getByText('Current')).toBeInTheDocument()
    expect(screen.getByText('$118,136.42')).toBeInTheDocument()
    expect(screen.getByText('Change')).toBeInTheDocument()
    expect(screen.getByText('+$136.42')).toBeInTheDocument()
    expect(screen.queryByText('(0.12%)')).not.toBeInTheDocument()
    expect(screen.queryByText('· simulated')).not.toBeInTheDocument()
    expect(screen.getByText('Time Left')).toBeInTheDocument()
    expect(screen.getByText('00:12')).toBeInTheDocument()

    await expectNoA11yViolations(container)
  })

  it('shows an explicit negative sign when the price is under the target', () => {
    render(<MarketPriceSummary {...base} price={117_800} />)

    expect(screen.getByText('-$200.00')).toBeInTheDocument()
    expect(screen.getByText('$117,800.00')).toBeInTheDocument()
    expect(screen.queryByText('(0.17%)')).not.toBeInTheDocument()
  })

  it('drops the sign when the cent amounts are equal', () => {
    render(<MarketPriceSummary {...base} price={118_000.004} />)

    expect(screen.getByText('$0.00')).toBeInTheDocument()
    expect(screen.queryByText('+$0.00')).not.toBeInTheDocument()
    expect(screen.queryByText('-$0.00')).not.toBeInTheDocument()
  })

  it('preserves a small negative change even when both prices round to the same dollar', () => {
    render(<MarketPriceSummary {...base} price={79_737.51} targetPrice={79_738.49} />)

    expect(screen.getByText('$79,737.51')).toBeInTheDocument()
    expect(screen.getByText('$79,738.49')).toBeInTheDocument()
    expect(screen.getByText('-$0.98')).toBeInTheDocument()
  })

  it('subtracts cents exactly across zero and never displays negative zero', () => {
    const { rerender } = render(<MarketPriceSummary {...base} price={0.29} targetPrice={0.3} />)
    expect(screen.getByText('-$0.01')).toBeInTheDocument()
    rerender(<MarketPriceSummary {...base} price={0.3} targetPrice={0.3} />)
    expect(screen.getByText('$0.00')).toBeInTheDocument()
    rerender(<MarketPriceSummary {...base} price={0.31} targetPrice={0.3} />)
    expect(screen.getByText('+$0.01')).toBeInTheDocument()
  })

  it('follows the price when it moves', () => {
    const { rerender } = render(<MarketPriceSummary {...base} />)

    expect(screen.getByText('+$136.42')).toBeInTheDocument()

    rerender(<MarketPriceSummary {...base} price={118_004} />)

    expect(screen.getByText('+$4.00')).toBeInTheDocument()
    expect(screen.queryByText('+$136.42')).not.toBeInTheDocument()
  })

  it('renders an unknown duration as unknown instead of inventing a countdown', () => {
    render(<MarketPriceSummary {...base} timeLeft={undefined} />)

    expect(screen.getByText('—')).toBeInTheDocument()
  })

  it('shows a disconnected feed as degraded', () => {
    render(<MarketPriceSummary {...base} feed={{ state: 'disconnected' }} />)

    expect(screen.getByText('Change · feed disconnected')).toBeInTheDocument()
    expect(screen.queryByText('Simulated data')).not.toBeInTheDocument()
  })

  it('says the same figures as one sentence when the price is over the target', () => {
    render(<MarketPriceSummary {...base} />)

    expect(screen.getByText('Up is ahead.').parentElement).toHaveTextContent(
      'Right now BTC is $136.42 above the price to beat. Up is ahead.'
    )
    expect(screen.queryByText('Down is ahead.')).not.toBeInTheDocument()
  })

  it('turns the sentence around when the price is under the target', () => {
    render(<MarketPriceSummary {...base} price={117_800} />)

    expect(screen.getByText('Down is ahead.').parentElement).toHaveTextContent(
      'Right now BTC is $200.00 below the price to beat. Down is ahead.'
    )
    expect(screen.queryByText('Up is ahead.')).not.toBeInTheDocument()
  })

  it('names no side when the cent amounts are equal', () => {
    render(<MarketPriceSummary {...base} price={118_000.004} />)

    expect(screen.getByText('level with').parentElement).toHaveTextContent(
      'Right now BTC is level with the price to beat.'
    )
    expect(screen.queryByText(/is ahead/)).not.toBeInTheDocument()
  })

  it.each(['disconnected', 'paused'] as const)(
    'does not call the last price from a %s feed "right now"',
    state => {
      render(<MarketPriceSummary {...base} feed={{ state }} />)

      expect(screen.queryByText(/Right now/)).not.toBeInTheDocument()
      expect(screen.queryByText(/is ahead/)).not.toBeInTheDocument()
      // The stats stay, labelled as stale.
      expect(screen.getByText('$118,136.42')).toBeInTheDocument()
    }
  )

  it('says nothing about a price it does not have', () => {
    render(<MarketPriceSummary {...base} price={Number.NaN} />)

    expect(screen.queryByText(/Right now/)).not.toBeInTheDocument()
    expect(screen.queryByText(/is ahead/)).not.toBeInTheDocument()
  })
})
