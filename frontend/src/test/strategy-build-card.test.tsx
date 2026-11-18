import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { expectNoA11yViolations } from '@tiny/testing/a11y'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest'
import { ChatMessages } from '@/features/market-detail/chat-messages'
import { CompileProposalCard } from '@/features/market-detail/compile-proposal-card'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

const brief = {
  series: 'btc-2m',
  summary: 'Buys up once the price crosses 60 cents and sells at a 5 cent gain.',
  goal: 'Buy up when it gets rich',
  entry: 'the up price crosses above 60 cents',
  signal_ids: ['btc-2m.current.price.v1'],
  outcome: 'up' as const,
  recurrence: 'once_per_round' as const,
  interval: null,
  entry_style: 'market' as const,
  exit: 'sell at +5 cents or cut at -10 cents',
  allocation: '100',
  round_share: '10%',
  trade_share: '25%',
  stop_after_loss: '100',
  stop_before_close: '10s',
  assumptions: ['Full size per order.'],
}
const steps = (done: number, running?: number) =>
  (['scout', 'plan', 'write', 'compile', 'review'] as const).map((id, index) => ({
    id,
    status: index < done ? 'done' : index === running ? 'running' : 'pending',
  }))
const compiled = {
  id: 'run-1',
  status: 'compiled',
  steps: steps(5),
  result: {
    readback: 'It buys up once the price crosses 60 cents and sells at a 5 cent gain.',
    assumptions: ['Full size per order.'],
    mismatches: [],
    warnings: [],
    problems: [],
    attempts: 1,
    strategy: { meta: { name: 'Threshold Entry' } },
    envelope: {
      allocation: '100',
      loss_budget: '100',
      round_budget: '10',
      max_order_size: '2',
      max_position_per_market: '10',
      max_open_orders: '4',
    },
    summary: {
      strategy_id: 'Strategy_abc',
      name: 'Threshold Entry',
      description: 'x',
      series: 'btc-2m',
      legs: ['hunting', 'holding', 'done'],
      subscriptions: ['btc-2m.current.price.v1'],
    },
  },
  error: null,
}

describe('compile proposal and build cards', () => {
  it('starts a build on Compile, shows background progress, then the readback', async () => {
    let polls = 0
    server.use(
      http.post('*/api/strategy/builds', async ({ request }) => {
        expect(await request.json()).toEqual({ brief })
        return HttpResponse.json({ id: 'run-1' }, { status: 202 })
      }),
      http.get('*/api/strategy/builds/run-1', () => {
        polls += 1
        return HttpResponse.json(
          polls < 2
            ? { id: 'run-1', status: 'running', steps: steps(1, 1), result: null, error: null }
            : compiled
        )
      })
    )
    const user = userEvent.setup()
    const onAsk = vi.fn()
    const { container } = render(
      <CompileProposalCard brief={brief} onAsk={onAsk} onKeepTalking={vi.fn()} />
    )
    expect(screen.getByText(brief.summary)).toBeVisible()
    expect(
      screen.getByText(
        '$100 in, sees $10 per round, up to $2.50 per order, stops after losing $100'
      )
    ).toBeVisible()
    await expectNoA11yViolations(container)
    await user.click(screen.getByRole('button', { name: 'Compile' }))
    // The step list is always in view while the build runs.
    expect(await screen.findByText('Writing the strategy…')).toBeVisible()
    expect(screen.getByText(/keep talking/i)).toBeVisible()
    // The card polls every 1.5 s; the second poll carries the finished build.
    expect(await screen.findByText('Strategy compiled', undefined, { timeout: 4000 })).toBeVisible()
    expect(screen.getByText(/sells at a 5 cent gain/)).toBeVisible()
    expect(screen.getByRole('button', { name: 'Deploy' })).toBeDisabled()
    await user.click(screen.getByRole('button', { name: 'Ask Teeny about this' }))
    expect(onAsk).toHaveBeenCalledWith(expect.stringContaining('It assumed: Full size per order.'))
    await expectNoA11yViolations(container)
  })

  it('offers to bring compiler problems back to Teeny when the build fails', async () => {
    server.use(
      http.post('*/api/strategy/builds', () => HttpResponse.json({ id: 'run-2' }, { status: 202 })),
      http.get('*/api/strategy/builds/run-2', () =>
        HttpResponse.json({
          id: 'run-2',
          status: 'failed',
          steps: [
            ...steps(3),
            { id: 'compile', status: 'failed' },
            { id: 'review', status: 'done' },
          ],
          result: {
            ...compiled.result,
            summary: null,
            readback: '',
            attempts: 4,
            problems: [
              'error[not-clamp-wrapped] at legs[0].body[0].do[0].place.size: a size must be clamp-wrapped',
            ],
          },
          error: null,
        })
      )
    )
    const user = userEvent.setup()
    const onAsk = vi.fn()
    render(<CompileProposalCard brief={brief} onAsk={onAsk} onKeepTalking={vi.fn()} />)
    await user.click(screen.getByRole('button', { name: 'Compile' }))
    expect(await screen.findByRole('alert')).toHaveTextContent('refused it after 4 attempts')
    await user.click(screen.getByRole('button', { name: 'Ask Teeny about this' }))
    expect(onAsk).toHaveBeenCalledWith(expect.stringContaining('not-clamp-wrapped'))
  })

  it('keeps the proposal when the build cannot be started', async () => {
    server.use(http.post('*/api/strategy/builds', () => new HttpResponse(null, { status: 503 })))
    const user = userEvent.setup()
    render(<CompileProposalCard brief={brief} onAsk={vi.fn()} onKeepTalking={vi.fn()} />)
    await user.click(screen.getByRole('button', { name: 'Compile' }))
    expect(await screen.findByRole('alert')).toHaveTextContent('could not be started')
    expect(screen.getByRole('button', { name: 'Compile' })).toBeEnabled()
  })

  it('shows the resting line from the route on a 429, starting no build', async () => {
    server.use(
      http.post('*/api/strategy/builds', () =>
        HttpResponse.json(
          {
            error: 'writer_resting',
            message: 'The strategy writer is resting until 12:00 AM UTC; templates still deploy',
          },
          { status: 429 }
        )
      )
    )
    const user = userEvent.setup()
    render(<CompileProposalCard brief={brief} onAsk={vi.fn()} onKeepTalking={vi.fn()} />)
    await user.click(screen.getByRole('button', { name: 'Compile' }))
    expect(await screen.findByRole('alert')).toHaveTextContent(
      'The strategy writer is resting until 12:00 AM UTC; templates still deploy'
    )
    expect(screen.getByRole('button', { name: 'Compile' })).toBeEnabled()
  })

  it('shows the not-switched-on line from the route on a 503 with a body', async () => {
    server.use(
      http.post('*/api/strategy/builds', () =>
        HttpResponse.json(
          {
            error: 'writer_unavailable',
            message: 'The strategy writer is not switched on here; templates still deploy.',
          },
          { status: 503 }
        )
      )
    )
    const user = userEvent.setup()
    render(<CompileProposalCard brief={brief} onAsk={vi.fn()} onKeepTalking={vi.fn()} />)
    await user.click(screen.getByRole('button', { name: 'Compile' }))
    expect(await screen.findByRole('alert')).toHaveTextContent(
      'The strategy writer is not switched on here; templates still deploy.'
    )
  })

  it('renders the proposal from the tool part and dismisses it on Keep talking', async () => {
    const user = userEvent.setup()
    render(
      <ChatMessages
        pending={false}
        failed={false}
        onRetry={vi.fn()}
        messages={[
          {
            id: 'm1',
            role: 'assistant',
            parts: [
              { type: 'text', text: 'Here is the plan.' },
              {
                type: 'tool-proposeCompilation',
                toolCallId: 'call-1',
                state: 'output-available',
                input: brief,
                output: { status: 'proposed', brief, summary: 'x' },
              },
            ],
          },
        ]}
      />
    )
    expect(screen.getByText(brief.summary)).toBeVisible()
    await waitFor(() => expect(screen.getByText('Here is the plan.')).toBeVisible())
    await user.click(screen.getByRole('button', { name: 'Keep talking' }))
    expect(screen.queryByText(brief.summary)).not.toBeInTheDocument()
  })
})
