import { render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { expectNoA11yViolations } from '@tiny/testing/a11y'
import { describe, expect, it } from 'vitest'

import {
  EdgeGaugeExample,
  FillLogExample,
  MarketCardExample,
  OrderBookExample,
  PositionCardExample,
  PriceVsStrikeExample,
  RoundResultsExample,
  SignalChartExample,
  SignalCompareExample,
  StatusListExample,
} from '@/features/chat/examples/preset-examples'

describe('EdgeGauge preset', () => {
  it('places the edge in its zone and says what it means', async () => {
    const { container } = render(<EdgeGaugeExample />)
    const meter = screen.getByRole('meter', { name: 'Edge' })
    expect(meter).toHaveAttribute('aria-valuenow', '3')
    expect(meter).toHaveAttribute('aria-valuetext', '+3¢, Buy YES')
    expect(screen.getByText('YES is underpriced by 3¢ against fair value.')).toBeVisible()
    await expectNoA11yViolations(container)
  })
})

describe('FillLog preset', () => {
  it('lists fills newest first and sorts by price on request', async () => {
    const user = userEvent.setup()
    const { container } = render(<FillLogExample />)
    const frame = within(screen.getByRole('region', { name: 'BTC 2m fill log' }))
    expect(frame.getByText('6 in this round')).toBeVisible()
    const firstRow = () => frame.getAllByRole('row')[1]?.textContent ?? ''
    expect(firstRow()).toContain('Sell YES')
    await user.click(frame.getByRole('button', { name: 'Price' }))
    expect(firstRow()).toContain('64¢')
    await user.click(frame.getByRole('button', { name: 'Price' }))
    expect(firstRow()).toContain('41¢')
    await expectNoA11yViolations(container)
  })
})

describe('RoundResults preset', () => {
  it('names each outcome and reports the streak', async () => {
    const { container } = render(<RoundResultsExample />)
    const frame = within(screen.getByRole('region', { name: 'BTC 2m recent round results' }))
    expect(frame.getByText('8 up, 4 down')).toBeVisible()
    expect(frame.getAllByRole('listitem')).toHaveLength(12)
    expect(frame.getByRole('listitem', { name: 'Down 54 dollars' })).toBeInTheDocument()
    expect(frame.getByText(/4 in a row up/)).toBeVisible()
    await expectNoA11yViolations(container)
  })
})

describe('StatusList preset', () => {
  it('counts the rules that are met and lists every rule', async () => {
    const { container } = render(<StatusListExample />)
    const frame = within(screen.getByRole('region', { name: 'Mean reversion v2 rules status' }))
    expect(frame.getByText('3 of 5 met')).toBeVisible()
    expect(frame.getAllByRole('row')).toHaveLength(6)
    expect(frame.getByText('Not met')).toBeVisible()
    await expectNoA11yViolations(container)
  })
})

describe('OrderBook preset', () => {
  it('derives imbalance and spread from the levels', async () => {
    const { container } = render(<OrderBookExample />)
    const frame = within(screen.getByRole('region', { name: 'BTC 2m order book' }))
    // Bids 7,055 against asks 4,836 over seven levels each.
    expect(frame.getByText(/^Imbalance \+0\.19$/)).toBeVisible()
    expect(frame.getByText('Spread 1¢')).toBeVisible()
    await expectNoA11yViolations(container)
  })
})

describe('PriceVsStrike preset', () => {
  it('shows the signed distance from the strike in the header', async () => {
    const { container } = render(<PriceVsStrikeExample />)
    const frame = within(screen.getByRole('region', { name: 'Bitcoin price against the strike' }))
    const header = frame.getByRole('heading', { name: 'BTC vs strike' }).parentElement
    expect(header).toHaveTextContent(/[+-]\$\d/)
    await expectNoA11yViolations(container)
  })
})

describe('SignalCompare preset', () => {
  it('lists both signals in the legend', async () => {
    const { container } = render(<SignalCompareExample />)
    const legend = within(screen.getByRole('list', { name: 'Series' }))
    expect(legend.getByText('EMA 9')).toBeVisible()
    expect(legend.getByText('EMA 21')).toBeVisible()
    await expectNoA11yViolations(container)
  })
})

describe('SignalChart preset', () => {
  it('names the signal, shows its live value and offers windows', async () => {
    const { container } = render(<SignalChartExample />)
    const frame = within(screen.getByRole('region', { name: 'RSI (14) signal chart' }))
    expect(frame.getByRole('heading', { name: 'RSI (14)' })).toBeVisible()
    expect(frame.getByRole('button', { name: '1m' })).toBeVisible()
    await expectNoA11yViolations(container)
  })
})

describe('PositionCard preset', () => {
  it('values the position under both resolutions', async () => {
    const { container } = render(<PositionCardExample />)
    const frame = within(screen.getByRole('region', { name: 'BTC 2m position' }))
    expect(frame.getByText('100 YES at 58¢')).toBeVisible()
    expect(frame.getByText('+$4.00')).toBeVisible()
    expect(frame.getByText('+$42.00')).toBeVisible()
    expect(frame.getByText('-$58.00')).toBeVisible()
    await expectNoA11yViolations(container)
  })
})

describe('MarketCard preset', () => {
  it('shows spot, distance from strike, countdown and the six stats', async () => {
    const { container } = render(<MarketCardExample />)
    const frame = within(screen.getByRole('region', { name: 'BTC 2m market' }))
    expect(frame.getByText('$72,783.05')).toBeVisible()
    expect(frame.getByText('+$28.91')).toBeVisible()
    expect(frame.getByText('T−0:47')).toBeVisible()
    expect(frame.getAllByRole('term').map(term => term.textContent)).toEqual([
      'Strike',
      'YES',
      'Spread',
      'Fair',
      'Market',
      'Edge',
    ])
    expect(frame.getByText('+3¢')).toBeVisible()
    await expectNoA11yViolations(container)
  })
})
