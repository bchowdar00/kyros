import { render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { describe, expect, it } from 'vitest'

import { MARKET_DETAIL_FIXTURES } from '@/features/market-detail/market-detail.fixture'
import { MyStrategiesPanel } from '@/features/market-detail/my-strategies-panel'

const myStrategies = MARKET_DETAIL_FIXTURES['btc-above-118k-0903'].myStrategies

describe('MyStrategiesPanel', () => {
  it('lets keyboard users reach the scrollable strategy list', async () => {
    const user = userEvent.setup()
    const { container } = render(<MyStrategiesPanel {...myStrategies} />)

    screen.getByRole('tabpanel', { name: 'My Accounts' }).focus()
    await user.tab()

    expect(screen.getByRole('region', { name: 'Scrollable accounts' })).toHaveFocus()
    await expectNoA11yViolations(container)
  })

  it('renders fixture-driven strategy states and P&L values', () => {
    render(<MyStrategiesPanel {...myStrategies} />)

    expect(screen.getByRole('region', { name: 'My Accounts' })).toBeInTheDocument()
    const table = screen.getByRole('table', { name: 'My accounts' })

    expect(within(table).getByText('Manual Trades')).toBeInTheDocument()
    expect(within(table).getByText('120 YES')).toBeInTheDocument()
    expect(within(table).getByText('$78.00')).toBeInTheDocument()
    expect(within(table).getByText('85 YES')).toBeInTheDocument()
    expect(within(table).getByText('$55.25')).toBeInTheDocument()
    expect(within(table).getByText('+$6.20')).toHaveClass('text-positive')
    expect(within(table).getByText('+$38.12')).toHaveClass('text-positive')
    expect(within(table).getByText('-$4.05')).toHaveClass('text-negative')
    expect(within(table).getByText('Paused')).toBeInTheDocument()
    expect(within(table).getAllByText('No Position')).toHaveLength(1)
  })

  it('starts on My Accounts with exchange orders available and supports tab switching', async () => {
    const user = userEvent.setup()

    render(<MyStrategiesPanel {...myStrategies} openOrders={<p>No open orders.</p>} />)

    const strategiesTab = screen.getByRole('tab', { name: 'My Accounts' })
    const ordersTab = screen.getByRole('tab', { name: 'Open Orders' })
    const historyTab = screen.getByRole('tab', { name: 'Trade History' })

    expect(strategiesTab).toHaveAttribute('aria-selected', 'true')
    expect(screen.getByRole('table', { name: 'My accounts' })).toBeInTheDocument()

    await user.click(ordersTab)

    expect(ordersTab).toHaveAttribute('aria-selected', 'true')
    expect(screen.queryByRole('table', { name: 'My accounts' })).not.toBeInTheDocument()
    expect(screen.getByRole('tabpanel', { name: 'Open Orders' })).toHaveTextContent(
      'No open orders.'
    )

    ordersTab.focus()
    await user.keyboard('{ArrowRight}')

    expect(historyTab).toHaveFocus()
    expect(historyTab).toHaveAttribute('aria-selected', 'true')
  })
})
