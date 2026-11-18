import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { expectNoA11yViolations } from '@tiny/testing/a11y'
import { http, HttpResponse } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
import type { StrategyBrief } from '@/mastra/capabilities/strategy/brief'
import { useIdentityStore } from '../identity/identity-store'
import { CompiledStrategyWidget } from './compiled-strategy-widget'
import type { BuildStatus } from './strategy-build'

const brief: StrategyBrief = {
  series: 'btc-2m',
  summary: 'Buys up once the price crosses 60 cents and sells at a 5 cent gain.',
  goal: 'Buy up when it gets rich',
  entry: 'the up price crosses above 60 cents',
  signal_ids: ['btc-2m.current.price.v1'],
  outcome: 'up',
  recurrence: 'once_per_round',
  interval: null,
  entry_style: 'market',
  exit: 'sell at +5 cents',
  allocation: '100',
  round_share: '10%',
  trade_share: '25%',
  stop_after_loss: '100',
  stop_before_close: '10s',
  assumptions: [],
}

const build: BuildStatus = {
  id: 'run-1',
  status: 'compiled',
  steps: [],
  error: null,
  result: {
    readback: 'It buys up once the price crosses 60 cents and sells at a 5 cent gain.',
    assumptions: [],
    mismatches: [],
    warnings: [],
    problems: [],
    attempts: 1,
    strategy: { version: '0.3', meta: { name: 'Threshold Entry' } },
    envelope: null,
    summary: {
      strategy_id: 'Strategy_abc',
      name: 'Threshold Entry',
      description: 'x',
      series: 'btc-2m',
      legs: ['hunting', 'done'],
      subscriptions: ['btc-2m.current.price.v1'],
    },
  },
}

const address = `0x${'ab'.repeat(20)}`
const exchange = { apiUrl: 'http://exchange.test', series: 'btc-2m' }
const server = setupServer()

beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())
beforeEach(() => {
  useIdentityStore.setState({
    identity: { address, username: 'jet', color: '#336699', emoji: '🦊' },
  })
})

function renderWidget(onClose = vi.fn()) {
  const view = render(
    <CompiledStrategyWidget
      build={build}
      brief={brief}
      scopeLabel="BTC Up or Down 2m"
      exchange={exchange}
      onClose={onClose}
    />
  )
  return { ...view, onClose, widget: screen.getByRole('dialog', { name: 'Deploy strategy' }) }
}

describe('compiled strategy widget', () => {
  it('shows the account, the readback and the money, and recomputes it as the allocation changes', async () => {
    const user = userEvent.setup()
    const { container, widget, onClose } = renderWidget()
    expect(within(widget).getByRole('heading', { name: 'Threshold Entry' })).toBeVisible()
    expect(within(widget).getByText('By jet')).toBeVisible()
    expect(within(widget).getByRole('tabpanel', { name: 'English' })).toHaveTextContent(
      build.result?.readback ?? ''
    )
    const allocation = within(widget).getByRole('textbox', { name: 'Total allocation in USD' })
    expect(allocation).toHaveValue('100.00')
    expect(within(widget).getByText(/per round/)).toHaveTextContent(
      'Sees $10 per round, up to $2.50 per order, stops after losing $100.'
    )
    expect(within(widget).getByRole('combobox', { name: 'Scope' })).toBeDisabled()
    expect(within(widget).getByRole('button', { name: 'Deploy strategy' })).toBeEnabled()
    expect(within(widget).getByRole('button', { name: 'Adjust bankroll' })).toHaveAttribute(
      'aria-expanded',
      'false'
    )
    await expectNoA11yViolations(container)

    await user.clear(allocation)
    await user.type(allocation, '200')
    await waitFor(() =>
      expect(within(widget).getByText(/per round/)).toHaveTextContent(
        'Sees $20 per round, up to $5 per order, stops after losing $100.'
      )
    )

    await user.clear(allocation)
    await user.type(allocation, '4')
    expect(await within(widget).findByRole('alert')).toHaveTextContent('less than one contract')
    expect(within(widget).getByRole('button', { name: 'Deploy strategy' })).toBeDisabled()

    await user.click(within(widget).getByRole('button', { name: 'Back to strategy composer' }))
    expect(onClose).toHaveBeenCalled()
  })

  it('opens the bankroll, keeps an edited budget as the allocation moves, and deploys it', async () => {
    const user = userEvent.setup()
    let posted: unknown
    server.use(
      http.post(`${exchange.apiUrl}/api/accounts/${address}/strategies`, async ({ request }) => {
        posted = await request.json()
        return HttpResponse.json(
          {
            id: `0x${'cd'.repeat(20)}`,
            kind: 'strategy',
            name: 'Threshold Entry',
            status: 'active',
            home_series: 'btc-2m',
            strategy: { series: 'btc-2m', strategy_id: 'Strategy_abc', deployment_id: 'x:1' },
          },
          { status: 201 }
        )
      })
    )
    const { container, widget, onClose } = renderWidget()
    await user.click(within(widget).getByRole('button', { name: 'Adjust bankroll' }))
    const round = within(widget).getByRole('textbox', { name: 'Per round budget in USD' })
    const order = within(widget).getByRole('textbox', { name: 'Per order budget in USD' })
    const stop = within(widget).getByRole('textbox', { name: 'Stop after losing in USD' })
    expect(round).toHaveValue('10.00')
    expect(order).toHaveValue('2.50')
    expect(stop).toHaveValue('100.00')
    await expectNoA11yViolations(container)

    await user.clear(round)
    await user.type(round, '20')
    await user.clear(stop)
    await user.type(stop, '50')
    await waitFor(() => expect(order).toHaveValue('5.00'))
    expect(within(widget).getByText(/per round/)).toHaveTextContent(
      'Sees $20 per round, up to $5 per order, stops after losing $50.'
    )

    const allocation = within(widget).getByRole('textbox', { name: 'Total allocation in USD' })
    await user.clear(allocation)
    await user.type(allocation, '200')
    await waitFor(() => expect(round).toHaveValue('20'))
    expect(order).toHaveValue('5.00')

    await user.click(within(widget).getByRole('button', { name: 'Deploy strategy' }))
    await waitFor(() => expect(onClose).toHaveBeenCalled())
    expect(posted).toEqual({
      allocation_tenths: 200_000,
      name: 'Threshold Entry',
      strategy: build.result?.strategy,
      envelope: {
        allocation: '200',
        loss_budget: '50',
        round_budget: '20',
        max_order_size: '5',
        max_position_per_market: '20',
        max_open_orders: '4',
      },
    })
  })

  it('shows the exchange’s refusal and stays open', async () => {
    const user = userEvent.setup()
    server.use(
      http.post(`${exchange.apiUrl}/api/accounts/${address}/strategies`, () =>
        HttpResponse.json(
          { error: 'insufficient_balance', message: 'insufficient balance' },
          { status: 400 }
        )
      )
    )
    const { widget, onClose } = renderWidget()
    await user.click(within(widget).getByRole('button', { name: 'Deploy strategy' }))
    expect(await within(widget).findByRole('alert')).toHaveTextContent(
      'insufficient_balance: insufficient balance'
    )
    expect(within(widget).getByRole('button', { name: 'Deploy strategy' })).toBeEnabled()
    expect(onClose).not.toHaveBeenCalled()
  })

  it('keeps Deploy inert without an exchange to post to', () => {
    render(
      <CompiledStrategyWidget
        build={build}
        brief={brief}
        scopeLabel="BTC Up or Down 2m"
        onClose={() => {}}
      />
    )
    expect(screen.getByRole('button', { name: 'Deploy strategy' })).toBeDisabled()
  })
})
