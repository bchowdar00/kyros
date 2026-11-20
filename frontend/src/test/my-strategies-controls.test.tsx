import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { MARKET_DETAIL_FIXTURES } from '@/features/market-detail/market-detail.fixture'
import { MyStrategiesPanel } from '@/features/market-detail/my-strategies-panel'

// Mock only the API client boundary; assert on rendered behaviour.
const { updateStrategyStatus, stopStrategy, getRecord } = vi.hoisted(() => ({
  updateStrategyStatus: vi.fn(),
  stopStrategy: vi.fn(),
  // Strategy rows mount a record line; a 404 keeps it off the screen.
  getRecord: vi.fn(async () => ({ ok: false, error: { status: 404 } })),
}))

vi.mock('@kyros/api', () => ({
  createRustExchangeClient: () => ({ updateStrategyStatus, stopStrategy, getRecord }),
}))

const myStrategies = MARKET_DETAIL_FIXTURES['btc-above-118k-0903'].myStrategies
const PARENT = `0x${'11'.repeat(20)}`
const API_URL = 'http://exchange.test'

// Panel order: [main account, active strategy, paused strategy].
const MAIN = 'Manual Trades'
const ACTIVE = 'Buy YES whenever funding flips positive'
const PAUSED = 'Fade breakouts above the strike in the last minute'

function renderPanel(
  props: { apiUrl?: string | undefined; parentAddress?: string | undefined } = {}
) {
  return render(
    <MyStrategiesPanel {...myStrategies} apiUrl={API_URL} parentAddress={PARENT} {...props} />
  )
}

beforeEach(() => {
  updateStrategyStatus.mockReset()
  stopStrategy.mockReset()
  getRecord.mockReset()
  getRecord.mockResolvedValue({ ok: false, error: { status: 404 } })
})

afterEach(() => {
  vi.clearAllTimers()
})

describe('MyStrategiesPanel strategy controls', () => {
  it('shows no controls on the main account row', () => {
    renderPanel()
    expect(screen.queryByRole('button', { name: `Pause ${MAIN}` })).toBeNull()
    expect(screen.queryByRole('button', { name: `Resume ${MAIN}` })).toBeNull()
    expect(screen.queryByRole('button', { name: `Stop ${MAIN}` })).toBeNull()
  })

  it('offers Pause and Stop on an active strategy, and Resume on a paused one', () => {
    renderPanel()
    expect(screen.getByRole('button', { name: `Pause ${ACTIVE}` })).toBeVisible()
    expect(screen.getByRole('button', { name: `Stop ${ACTIVE}` })).toBeVisible()
    expect(screen.getByRole('button', { name: `Resume ${PAUSED}` })).toBeVisible()
    expect(screen.queryByRole('button', { name: `Pause ${PAUSED}` })).toBeNull()
  })

  it('hides the controls when no parent address is available', () => {
    renderPanel({ parentAddress: undefined })
    expect(screen.queryByRole('button', { name: `Pause ${ACTIVE}` })).toBeNull()
    expect(screen.queryByRole('button', { name: `Stop ${ACTIVE}` })).toBeNull()
  })

  it('flips the badge to Paused on a successful pause without a reload', async () => {
    const user = userEvent.setup()
    updateStrategyStatus.mockResolvedValue({ ok: true, data: { status: 'paused' } })
    renderPanel()

    expect(screen.getAllByText('Paused')).toHaveLength(1)
    await user.click(screen.getByRole('button', { name: `Pause ${ACTIVE}` }))

    expect(updateStrategyStatus).toHaveBeenCalledWith(PARENT, 'funding-flips', 'paused')
    await waitFor(() => expect(screen.getAllByText('Paused')).toHaveLength(2))
    expect(screen.getByRole('button', { name: `Resume ${ACTIVE}` })).toBeVisible()
  })

  it('shows an alert and keeps the state when a pause fails', async () => {
    const user = userEvent.setup()
    updateStrategyStatus.mockResolvedValue({ ok: false, error: { status: 500 } })
    renderPanel()

    await user.click(screen.getByRole('button', { name: `Pause ${ACTIVE}` }))

    const alert = await screen.findByRole('alert')
    expect(alert).toHaveTextContent('Could not pause the strategy. Try again.')
    expect(screen.getByRole('button', { name: `Pause ${ACTIVE}` })).toBeVisible()
  })

  it('opens a confirmation on Stop and calls nothing until Stop strategy', async () => {
    const user = userEvent.setup()
    stopStrategy.mockResolvedValue({
      ok: true,
      data: { removed: true, swept_tenths: 1000, cancelled_orders: 1, residual_position: 0 },
    })
    renderPanel()

    await user.click(screen.getByRole('button', { name: `Stop ${ACTIVE}` }))

    expect(screen.getByText(/Its open orders are cancelled/)).toBeVisible()
    expect(stopStrategy).not.toHaveBeenCalled()

    await user.click(screen.getByRole('button', { name: 'Stop strategy' }))
    expect(stopStrategy).toHaveBeenCalledWith(PARENT, 'funding-flips')
  })

  it('closes the confirmation and returns focus to Stop on Keep it running', async () => {
    const user = userEvent.setup()
    renderPanel()

    const stopButton = screen.getByRole('button', { name: `Stop ${ACTIVE}` })
    await user.click(stopButton)
    expect(screen.getByText(/Its open orders are cancelled/)).toBeVisible()

    await user.click(screen.getByRole('button', { name: 'Keep it running' }))

    expect(screen.queryByText(/Its open orders are cancelled/)).toBeNull()
    expect(stopStrategy).not.toHaveBeenCalled()
    expect(stopButton).toHaveFocus()
  })

  it('removes the row when the stop reports the sub-account is gone', async () => {
    const user = userEvent.setup()
    stopStrategy.mockResolvedValue({
      ok: true,
      data: { removed: true, swept_tenths: 49000, cancelled_orders: 2, residual_position: 0 },
    })
    renderPanel()

    await user.click(screen.getByRole('button', { name: `Stop ${ACTIVE}` }))
    await user.click(screen.getByRole('button', { name: 'Stop strategy' }))

    await waitFor(() => expect(screen.queryByText(ACTIVE)).toBeNull())
  })

  it('says so above the table when the position could not be closed, and takes the controls off the row', async () => {
    const user = userEvent.setup()
    stopStrategy.mockResolvedValue({
      ok: true,
      data: { removed: false, swept_tenths: 0, cancelled_orders: 2, residual_position: 40 },
    })
    renderPanel()

    await user.click(screen.getByRole('button', { name: `Stop ${ACTIVE}` }))
    await user.click(screen.getByRole('button', { name: 'Stop strategy' }))

    const status = await screen.findByRole('status')
    expect(status).toHaveTextContent(
      `Stopped ${ACTIVE}. Its orders are cancelled. Its position could not be closed now, so it settles with the round, and its money returns to your paper balance then.`
    )
    expect(screen.queryByRole('button', { name: `Pause ${ACTIVE}` })).toBeNull()
    expect(screen.queryByRole('button', { name: `Stop ${ACTIVE}` })).toBeNull()
    expect(screen.queryByText(/Its open orders are cancelled and it places no more/)).toBeNull()
  })

  it('keeps that notice after the exchange drops the stopped row, until it is dismissed', async () => {
    const user = userEvent.setup()
    stopStrategy.mockResolvedValue({
      ok: true,
      data: { removed: false, swept_tenths: 0, cancelled_orders: 0, residual_position: -3 },
    })
    const { rerender } = renderPanel()

    await user.click(screen.getByRole('button', { name: `Stop ${ACTIVE}` }))
    await user.click(screen.getByRole('button', { name: 'Stop strategy' }))
    await screen.findByRole('status')

    rerender(
      <MyStrategiesPanel
        {...myStrategies}
        apiUrl={API_URL}
        parentAddress={PARENT}
        rows={myStrategies.rows.filter(row => row.name !== ACTIVE)}
      />
    )

    expect(screen.queryByRole('rowheader', { name: new RegExp(ACTIVE) })).toBeNull()
    expect(screen.getByRole('status')).toHaveTextContent(`Stopped ${ACTIVE}.`)

    await user.click(screen.getByRole('button', { name: /^Dismiss/ }))
    expect(screen.queryByRole('status')).toBeNull()
  })

  it('keeps the notice while the visitor looks at another tab', async () => {
    const user = userEvent.setup()
    stopStrategy.mockResolvedValue({
      ok: true,
      data: { removed: false, swept_tenths: 0, cancelled_orders: 1, residual_position: 12 },
    })
    renderPanel()

    await user.click(screen.getByRole('button', { name: `Stop ${ACTIVE}` }))
    await user.click(screen.getByRole('button', { name: 'Stop strategy' }))
    await screen.findByRole('status')

    await user.click(screen.getByRole('tab', { name: 'Open Orders' }))
    await user.click(screen.getByRole('tab', { name: 'My Accounts' }))

    expect(screen.getByRole('status')).toHaveTextContent(`Stopped ${ACTIVE}.`)
  })

  it('keeps one notice for each strategy stopped that way', async () => {
    const user = userEvent.setup()
    stopStrategy.mockResolvedValue({
      ok: true,
      data: { removed: false, swept_tenths: 0, cancelled_orders: 0, residual_position: 5 },
    })
    renderPanel()

    await user.click(screen.getByRole('button', { name: `Stop ${ACTIVE}` }))
    await user.click(screen.getByRole('button', { name: 'Stop strategy' }))
    await screen.findByRole('status')
    await user.click(screen.getByRole('button', { name: `Stop ${PAUSED}` }))
    await user.click(screen.getByRole('button', { name: 'Stop strategy' }))

    await waitFor(() => expect(screen.getAllByRole('status')).toHaveLength(2))
    await user.click(screen.getByRole('button', { name: `Dismiss: the notice about ${ACTIVE}` }))

    const [left] = screen.getAllByRole('status')
    expect(screen.getAllByRole('status')).toHaveLength(1)
    expect(left).toHaveTextContent(`Stopped ${PAUSED}.`)
  })

  it('shows an alert when the stop fails, without claiming what the server did', async () => {
    const user = userEvent.setup()
    stopStrategy.mockResolvedValue({ ok: false, error: { status: 500 } })
    renderPanel()

    await user.click(screen.getByRole('button', { name: `Stop ${ACTIVE}` }))
    await user.click(screen.getByRole('button', { name: 'Stop strategy' }))

    const alert = await screen.findByRole('alert')
    expect(alert).toHaveTextContent('Could not stop the strategy. Try again.')
    expect(alert).not.toHaveTextContent('Nothing changed')
    expect(screen.getByText(ACTIVE)).toBeVisible()
  })

  it('gives the buttons back when a request throws', async () => {
    const user = userEvent.setup()
    stopStrategy.mockRejectedValue(new Error('socket hang up'))
    updateStrategyStatus.mockRejectedValue(new Error('socket hang up'))
    renderPanel()

    await user.click(screen.getByRole('button', { name: `Pause ${ACTIVE}` }))
    expect(await screen.findByRole('alert')).toHaveTextContent(
      'Could not pause the strategy. Try again.'
    )
    expect(screen.getByRole('button', { name: `Pause ${ACTIVE}` })).toBeEnabled()

    await user.click(screen.getByRole('button', { name: `Stop ${ACTIVE}` }))
    await user.click(screen.getByRole('button', { name: 'Stop strategy' }))
    expect(await screen.findByRole('alert')).toHaveTextContent(
      'Could not stop the strategy. Try again.'
    )
    expect(screen.getByRole('button', { name: `Stop ${ACTIVE}` })).toBeEnabled()
  })

  it('sends one request however fast Pause is pressed', async () => {
    const user = userEvent.setup()
    const settle: { now?: (value: unknown) => void } = {}
    updateStrategyStatus.mockReturnValue(
      new Promise(resolve => {
        settle.now = resolve
      })
    )
    renderPanel()

    const pause = screen.getByRole('button', { name: `Pause ${ACTIVE}` })
    await user.click(pause)
    await user.click(pause)
    expect(updateStrategyStatus).toHaveBeenCalledTimes(1)
    expect(pause).toBeDisabled()

    settle.now?.({ ok: true, data: { status: 'paused' } })
    await waitFor(() =>
      expect(screen.getByRole('button', { name: `Resume ${ACTIVE}` })).toBeEnabled()
    )
  })

  it('says what stopping does in words that hold when the position cannot be sold', async () => {
    const user = userEvent.setup()
    renderPanel()

    await user.click(screen.getByRole('button', { name: `Stop ${ACTIVE}` }))

    expect(
      screen.getByText(
        `Stop ${ACTIVE}? Its open orders are cancelled and it places no more. Its position is closed at the market when that is possible; otherwise it settles with the round. Its money then returns to your paper balance. Rounds already sealed on Arc stay there.`
      )
    ).toBeVisible()
  })

  it('has no accessibility violations with the controls rendered', async () => {
    const { container } = renderPanel()
    await expectNoA11yViolations(container)
  })
})
