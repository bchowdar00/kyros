import { cleanup, render, screen, waitFor } from '@testing-library/react'
import { http, HttpResponse } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'

import { LivePublicStrategiesList } from './live-public-strategies-list'
import { PUBLIC_STRATEGY_FIXTURES } from './public-strategies.fixture'

const server = setupServer()
const api = { apiUrl: 'http://exchange.test', series: 'btc-15m' }
const [first, second] = PUBLIC_STRATEGY_FIXTURES
const loaded = [first, second]

function card(id: string, waiting: { signal_id: string; name: string }[]) {
  const side = { name: id, logic: `${id} prose`, running: 1, deployed_tenths: 0 }
  return {
    id,
    title: `${id} title`,
    series: ['btc-15m'],
    creator: { handle: '@kyros', username: 'Kyros', avatar: null },
    envelope: { loss_budget_fraction: '0.5', max_order_size: '50' },
    primary: { ...side, ready: waiting.length === 0, waiting_on: waiting },
    inverse: { ...side, ready: true, waiting_on: [] },
    running: 2,
    deployed_tenths: 0,
    runners: [],
    ready: waiting.length === 0,
    waiting_on: waiting,
  }
}

beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  cleanup()
  server.resetHandlers()
})
afterAll(() => server.close())

describe('live public strategies list', () => {
  it('shows the loaded cards, then only the ones the exchange says can deploy', async () => {
    server.use(
      http.get('*/api/strategies/templates', ({ request }) => {
        expect(new URL(request.url).searchParams.get('series')).toBe('btc-15m')
        return HttpResponse.json({
          series: 'btc-15m',
          templates: [
            card('late-closer', []),
            card('trend-rider', [
              { signal_id: 'btc.external.horizon_momentum.v1', name: 'Momentum' },
            ]),
          ],
        })
      })
    )
    render(<LivePublicStrategiesList api={api} initial={loaded} />)

    for (const strategy of loaded) {
      expect(screen.getByText(strategy.title)).toBeInTheDocument()
    }
    await waitFor(() => expect(screen.getByText('late-closer title')).toBeInTheDocument())
    expect(screen.queryByText('trend-rider title')).not.toBeInTheDocument()
    expect(screen.queryByText(first.title)).not.toBeInTheDocument()
  })

  it('keeps the loaded cards when the exchange does not answer', async () => {
    let asked = false
    server.use(
      http.get('*/api/strategies/templates', () => {
        asked = true
        return HttpResponse.json({ error: 'unavailable', message: 'nope' }, { status: 503 })
      })
    )
    render(<LivePublicStrategiesList api={api} initial={loaded} />)

    await waitFor(() => expect(asked).toBe(true))
    expect(screen.getByText(first.title)).toBeInTheDocument()
    expect(screen.getByText(second.title)).toBeInTheDocument()
  })
})
