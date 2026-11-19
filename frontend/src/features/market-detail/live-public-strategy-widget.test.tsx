import { cleanup, render, screen, waitFor } from '@testing-library/react'
import { http, HttpResponse } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'

import { LivePublicStrategyWidget } from './live-public-strategy-widget'
import { PUBLIC_STRATEGY_FIXTURES } from './public-strategies.fixture'

const server = setupServer()
const api = { apiUrl: 'http://exchange.test', series: 'btc-5m' }
const loaded = { ...PUBLIC_STRATEGY_FIXTURES[0], templateId: 'fair-value-discount' }
const fresh = {
  id: 'fair-value-discount',
  title: loaded.title,
  series: ['btc-5m'],
  creator: { handle: '@kyros', username: 'Kyros', avatar: null },
  envelope: { loss_budget_fraction: '0.5', max_order_size: '50' },
  primary: {
    name: 'Fair Value Discount',
    logic: 'Fresh Up prose.',
    running: 2,
    deployed_tenths: 0,
  },
  inverse: {
    name: 'Fair Value Discount (Down)',
    logic: 'Fresh Down prose.',
    running: 5,
    deployed_tenths: 0,
  },
  running: 7,
  deployed_tenths: 2_000_000,
  runners: [],
}

beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  cleanup()
  server.resetHandlers()
})
afterAll(() => server.close())

describe('live public strategy widget', () => {
  it('shows the loaded card, then the exchange’s answer for the requested series', async () => {
    server.use(
      http.get('*/api/strategies/templates/fair-value-discount', ({ request }) => {
        expect(new URL(request.url).searchParams.get('series')).toBe('btc-5m')
        return HttpResponse.json(fresh)
      })
    )
    render(
      <LivePublicStrategyWidget
        api={api}
        strategy={loaded}
        inverse
        scopeLabel="BTC 5m"
        onClose={() => {}}
      />
    )

    expect(screen.getByText(loaded.inverseLogic)).toBeInTheDocument()
    await waitFor(() => expect(screen.getByText('Fresh Down prose.')).toBeInTheDocument())
    expect(screen.getByText('7')).toBeInTheDocument()
    // Two million tenths of a cent is $2,000.
    expect(screen.getByText('$2K')).toBeInTheDocument()
    expect(screen.queryByRole('status')).not.toBeInTheDocument()
  })

  it('keeps the loaded card and says so when the exchange does not answer', async () => {
    server.use(
      http.get('*/api/strategies/templates/fair-value-discount', () =>
        HttpResponse.json({ error: 'template_not_found', message: 'nope' }, { status: 404 })
      )
    )
    render(
      <LivePublicStrategyWidget
        api={api}
        strategy={loaded}
        inverse={false}
        scopeLabel="BTC 5m"
        onClose={() => {}}
      />
    )

    await waitFor(() => expect(screen.getByRole('status')).toHaveTextContent('did not answer'))
    expect(screen.getByText(loaded.logic)).toBeInTheDocument()
  })
})
