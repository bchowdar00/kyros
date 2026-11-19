import { cents } from '@kyros/money'
import { http, HttpResponse } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'

import { FakeWebSocket } from '@/features/exchange-streams/fake-web-socket.fixture'

import { createRustExchangeStore } from './rust-exchange-store'

const server = setupServer()
const address = `0x${'ab'.repeat(20)}`
const series = 'btc-5m'

function socket(path: string) {
  const found = FakeWebSocket.instances.find(instance => new URL(instance.url).pathname === path)
  if (found === undefined) throw new Error(`no socket for ${path}`)
  return found
}

function liveStore() {
  const store = createRustExchangeStore({ apiUrl: 'http://exchange.test', address, series })
  const unsubscribe = store.subscribe(() => undefined)
  const closesAt = Date.now() + 60_000
  socket('/ws/price').emit({
    type: 'snapshot',
    series,
    duration_secs: 300,
    market_id: 7,
    strike: null,
    opens_at_ms: closesAt - 300_000,
    closes_at_ms: closesAt,
    ticks: [],
  })
  socket('/ws/book').emit({
    type: 'snapshot',
    series,
    bids: [{ price: 499, qty: 10 }],
    asks: [{ price: 501, qty: 10 }],
  })
  socket('/ws/positions').emit({
    type: 'position_snapshot',
    as_of_unix_ms: 1,
    series,
    market_id: 7,
    address,
    side: 'flat',
    size_shares: 0,
    signed_position: 0,
    avg_entry_yes_tenths: null,
    cost_basis_tenths: 0,
    mark_yes_tenths: null,
    unrealized_pnl_tenths: null,
    realized_pnl_this_market_tenths: 0,
    balance_tenths: 1_000_000,
    locked_tenths: 0,
    series_positions: [],
  })
  return { store, unsubscribe }
}

beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
beforeEach(() => vi.stubGlobal('WebSocket', FakeWebSocket))
afterEach(() => {
  server.resetHandlers()
  FakeWebSocket.reset()
  vi.unstubAllGlobals()
})
afterAll(() => server.close())

const quote = {
  marketId: '7',
  side: 'buy' as const,
  price: 501,
  outcomePrice: 501,
  qty: '1',
  reserve: '501000',
  payoutCents: cents(100),
}
const liveBlock = {
  env: 'production' as const,
  series: 'BTC05M',
  phase: 'open' as const,
  book: 'live' as const,
}

describe('rust exchange store', () => {
  it('reads a refused order back as a sentence', async () => {
    server.use(
      http.post('*/api/orders', () =>
        HttpResponse.json(
          { error: 'market_closed', message: 'market 7 is closed' },
          { status: 409 }
        )
      )
    )
    const { store, unsubscribe } = liveStore()

    await store.buy(
      {
        marketId: '7',
        side: 'buy',
        price: 501,
        outcomePrice: 501,
        qty: '1',
        reserve: '501000',
        payoutCents: cents(100),
      },
      {
        status: 'polled',
        gemini: { env: 'production', series: 'BTC05M', phase: 'open', book: 'live' },
      }
    )

    expect(store.getSnapshot().notice).toBe(
      'Order not placed. This round is closed to new orders. The next one opens shortly.'
    )
    unsubscribe()
  })

  it('says the exchange did not confirm an order whose answer breaks the contract', async () => {
    server.use(http.post('*/api/orders', () => HttpResponse.json({ unexpected: true })))
    const { store, unsubscribe } = liveStore()

    await store.buy(quote, { status: 'polled', gemini: liveBlock })

    expect(store.getSnapshot().notice).toBe(
      'The exchange did not confirm this order. Check Open Orders and Trade History before trying again.'
    )
    unsubscribe()
  })

  it('does not take a worded server error as proof the order was not placed', async () => {
    server.use(
      http.post('*/api/orders', () =>
        HttpResponse.json({ error: 'internal', message: 'boom' }, { status: 500 })
      )
    )
    const { store, unsubscribe } = liveStore()

    await store.buy(quote, { status: 'polled', gemini: liveBlock })

    expect(store.getSnapshot().notice).toBe(
      'The exchange did not confirm this order. Check Open Orders and Trade History before trying again.'
    )
    unsubscribe()
  })

  it('says the exchange did not confirm a deployment whose answer breaks the contract', async () => {
    server.use(
      http.post('*/api/accounts/:address/templates/:id/deploy', () =>
        HttpResponse.json({ unexpected: true })
      )
    )
    const { store, unsubscribe } = liveStore()

    const deployed = await store.deployTemplate({
      templateId: 'fair-value-discount',
      inverse: false,
      allocationCents: cents(4000),
    })

    expect(deployed).toBe(false)
    expect(store.getSnapshot().deployNotice).toBe(
      'The exchange did not confirm this deployment. Check My Accounts before trying again.'
    )
    unsubscribe()
  })

  it("reads a refused deployment back in the exchange's words", async () => {
    server.use(
      http.post('*/api/accounts/:address/templates/:id/deploy', () =>
        HttpResponse.json(
          { error: 'insufficient_balance', message: 'insufficient balance' },
          { status: 422 }
        )
      )
    )
    const { store, unsubscribe } = liveStore()

    await store.deployTemplate({
      templateId: 'fair-value-discount',
      inverse: false,
      allocationCents: cents(4000),
    })

    expect(store.getSnapshot().deployNotice).toBe('Not deployed. Insufficient balance.')
    unsubscribe()
  })

  it('says the exchange did not confirm a cancel that got no readable answer', async () => {
    server.use(
      http.delete(
        '*/api/orders/:id',
        () => new HttpResponse('<html>bad gateway</html>', { status: 502 })
      )
    )
    const { store, unsubscribe } = liveStore()

    await store.cancel({
      series,
      order_id: 9,
      market_id: 7,
      account: address,
      account_kind: 'main',
      account_name: 'Main',
      side: 'bid',
      price: 501,
      qty_open: 1,
      placed_ts: 1,
    })

    expect(store.getSnapshot().notice).toBe(
      'The exchange did not confirm the cancel. Check Open Orders before trying again.'
    )
    unsubscribe()
  })

  it("sends nothing while Gemini's book could not be confirmed", async () => {
    // No handler: with onUnhandledRequest 'error', any request would fail the test.
    const { store, unsubscribe } = liveStore()

    await store.buy(quote, { status: 'poll-failed', gemini: liveBlock })

    expect(store.getSnapshot().notice).toBe(
      'The market changed or is unavailable. Review the current quote.'
    )
    expect(store.getSnapshot().pending).toBe(false)
    unsubscribe()
  })

  it("refuses to send an order while Gemini's book is quiet", async () => {
    const { store, unsubscribe } = liveStore()
    await store.buy(
      {
        marketId: '7',
        side: 'buy',
        price: 501,
        outcomePrice: 501,
        qty: '1',
        reserve: '501000',
        payoutCents: cents(100),
      },
      {
        status: 'polled',
        gemini: { env: 'production', series: 'BTC05M', phase: 'open', book: 'stale' },
      }
    )
    expect(store.getSnapshot().notice).toBe(
      'The market changed or is unavailable. Review the current quote.'
    )
    unsubscribe()
  })
})
