import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'

import { createRustExchangeClient } from './rust-exchange'

const server = setupServer()
const baseUrl = 'http://exchange.test'
const address = `0x${'12'.repeat(20)}`

beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

describe('rust exchange client', () => {
  it('lists series and places a limit order on the requested series', async () => {
    server.use(
      http.get(`${baseUrl}/api/series`, () =>
        HttpResponse.json([
          {
            series: 'btc-2m',
            duration_secs: 120,
            current_market_id: 7,
            opens_at_ms: 10,
            closes_at_ms: 120_010,
          },
        ])
      ),
      http.post(`${baseUrl}/api/orders`, async ({ request }) => {
        expect(await request.json()).toEqual({
          type: 'limit',
          address,
          side: 'bid',
          price: 520,
          qty: 3,
          series: 'btc-15m',
        })
        return HttpResponse.json(
          { order_id: 5, fills: [], status: 'rested', resting_qty: 3 },
          { status: 201 }
        )
      })
    )
    const client = createRustExchangeClient({ baseUrl, retry: { attempts: 1 } })
    const series = await client.listSeries()
    expect(series.ok && series.data[0]?.current_market_id).toBe(7)
    const order = await client.placeOrder({
      address,
      series: 'btc-15m',
      side: 'bid',
      price: 520,
      qty: 3,
    })
    expect(order).toEqual({
      ok: true,
      data: { order_id: 5, fills: [], status: 'rested', resting_qty: 3 },
    })
  })

  it('surfaces the exchange error code on a rejected order and cancels with query params', async () => {
    server.use(
      http.post(`${baseUrl}/api/orders`, () =>
        HttpResponse.json(
          { error: 'insufficient_balance', message: 'insufficient balance' },
          { status: 400 }
        )
      ),
      http.delete(`${baseUrl}/api/orders/5`, ({ request }) => {
        const query = new URL(request.url).searchParams
        expect(query.get('address')).toBe(address)
        expect(query.get('series')).toBe('btc-2m')
        return new HttpResponse(null, { status: 204 })
      })
    )
    const client = createRustExchangeClient({ baseUrl, retry: { attempts: 1 } })
    const order = await client.placeOrder({ address, series: 'btc-2m', side: 'ask', qty: 1 })
    expect(order.ok).toBe(false)
    if (!order.ok) {
      expect(order.error.status).toBe(400)
      expect(order.error.message).toBe('insufficient_balance: insufficient balance')
    }
    expect(await client.cancelOrder(5, address, 'btc-2m')).toEqual({ ok: true, data: undefined })
  })

  it('reads signal history, price history, the book, fills and positions with wire query names', async () => {
    const frame = {
      signal_id: 'btc.coinbase.rsi.v1',
      ts_ms: 5,
      quality: { state: 'ready', reasons: [] },
      values: { rsi_14: '38.2' },
    }
    server.use(
      http.get(`${baseUrl}/api/signals/history`, ({ request }) => {
        const query = new URL(request.url).searchParams
        expect(query.get('series')).toBe('btc-2m')
        expect(query.get('ids')).toBe('btc.coinbase.rsi.v1,pyth.btc_usd.spot.v1')
        expect(query.get('history_ms')).toBe('240000')
        expect(query.get('max_points')).toBe('50')
        return HttpResponse.json({
          series: 'btc-2m',
          since_ms: 0,
          signals: [{ signal_id: 'btc.coinbase.rsi.v1', frames: [frame] }],
        })
      }),
      http.get(`${baseUrl}/api/price/history`, ({ request }) => {
        expect(new URL(request.url).searchParams.get('max_points')).toBe('100')
        return HttpResponse.json({
          type: 'snapshot',
          series: 'btc-2m',
          duration_secs: 120,
          market_id: 3,
          strike: 79_650.5,
          opens_at_ms: 1,
          closes_at_ms: 120_001,
          ticks: [{ ts_ms: 2, price: 79_651 }],
        })
      }),
      http.get(`${baseUrl}/api/book`, ({ request }) => {
        expect(new URL(request.url).searchParams.get('depth')).toBe('15')
        return HttpResponse.json({ bids: [{ price: 500, qty: 3 }], asks: [] })
      }),
      http.get(`${baseUrl}/api/trades`, () =>
        HttpResponse.json([
          {
            taker: address,
            maker: address,
            taker_side: 'bid',
            price: 510,
            qty: 2,
            ts: 9,
            kind: 'open',
          },
        ])
      ),
      http.get(`${baseUrl}/api/accounts/${address}/trades`, () =>
        HttpResponse.json({ trades: [] })
      ),
      http.get(`${baseUrl}/api/accounts/${address}/positions`, ({ request }) => {
        expect(new URL(request.url).searchParams.get('series')).toBe('btc-15m')
        return HttpResponse.json({
          type: 'position_snapshot',
          as_of_unix_ms: 9,
          series: 'btc-15m',
          market_id: 3,
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
      })
    )
    const client = createRustExchangeClient({ baseUrl, retry: { attempts: 1 } })
    const history = await client.getSignalsHistory({
      series: 'btc-2m',
      ids: ['btc.coinbase.rsi.v1', 'pyth.btc_usd.spot.v1'],
      historyMs: 240_000,
      maxPoints: 50,
    })
    expect(history.ok && history.data.signals[0]?.frames[0]?.values['rsi_14']).toBe('38.2')
    const price = await client.getPriceHistory('btc-2m', 100)
    expect(price.ok && price.data.ticks).toEqual([{ ts_ms: 2, price: 79_651 }])
    const book = await client.getBook('btc-2m', 15)
    expect(book.ok && book.data.bids[0]?.price).toBe(500)
    const fills = await client.getTrades('btc-2m')
    expect(fills.ok && fills.data[0]?.kind).toBe('open')
    expect(await client.getUserTrades(address)).toEqual({ ok: true, data: { trades: [] } })
    const positions = await client.getPositions(address, 'btc-15m')
    expect(positions.ok && positions.data.side).toBe('flat')
  })

  it('reads the strategy template catalogue and one template for a series', async () => {
    const template = {
      id: 'fair-value-discount',
      title: 'Buy the side the model says is cheap',
      series: ['btc-2m', 'btc-15m'],
      creator: { handle: '@tiny', username: 'Tiny', avatar: 'strategy-01.jpg' },
      envelope: { loss_budget_fraction: '0.5', max_order_size: '50' },
      primary: {
        name: 'Fair Value Discount',
        logic: 'Buy Up.',
        running: 0,
        deployed_tenths: 0,
        ready: true,
        waiting_on: [],
      },
      inverse: {
        name: 'Fair Value Discount (Down)',
        logic: 'Buy Down.',
        running: 0,
        deployed_tenths: 0,
        ready: false,
        waiting_on: [{ signal_id: 'btc-2m.current.window.v1', name: 'Round Clock' }],
      },
      running: 0,
      deployed_tenths: 0,
      runners: [],
      ready: false,
      waiting_on: [{ signal_id: 'btc-2m.current.window.v1', name: 'Round Clock' }],
    }
    server.use(
      http.get(`${baseUrl}/api/strategies/templates`, ({ request }) => {
        expect(new URL(request.url).searchParams.get('series')).toBe('btc-15m')
        return HttpResponse.json({ series: 'btc-15m', templates: [template] })
      }),
      http.get(`${baseUrl}/api/strategies/templates/fair-value-discount`, ({ request }) => {
        expect(new URL(request.url).searchParams.get('series')).toBe('btc-2m')
        return HttpResponse.json(template)
      }),
      http.get(`${baseUrl}/api/strategies/templates/trend-rider`, () =>
        HttpResponse.json({ error: 'template_not_found', message: 'not listed' }, { status: 404 })
      )
    )
    const client = createRustExchangeClient({ baseUrl, retry: { attempts: 1 } })
    const list = await client.getStrategyTemplates('btc-15m')
    expect(list.ok && list.data.templates[0]?.creator.username).toBe('Tiny')
    const one = await client.getStrategyTemplate('fair-value-discount', 'btc-2m')
    expect(one.ok && one.data.inverse.logic).toBe('Buy Down.')
    expect(one.ok && one.data.ready).toBe(false)
    expect(one.ok && one.data.waiting_on?.map(signal => signal.name)).toEqual(['Round Clock'])
    const missing = await client.getStrategyTemplate('trend-rider', 'btc-2m')
    expect(!missing.ok && missing.error.status).toBe(404)
  })

  it('deploys a template with its idempotency key, retrying only once keyed', async () => {
    const child = `0x${'ab'.repeat(20)}`
    const detail = {
      id: child,
      kind: 'strategy',
      name: 'Late Closer',
      status: 'active',
      created_at_ms: 1_700_000_000_000,
      home_series: 'btc-15m',
      trades_this_market: 0,
      series_positions: [],
      balance_tenths: 50_000,
      locked_tenths: 0,
      equity_tenths: 50_000,
      net_contributed_tenths: 50_000,
      lifetime_pnl_tenths: 0,
      lifetime_realized_pnl_tenths: 0,
      strategy: {
        name: 'Late Closer',
        status: 'active',
        created_at_ms: 1_700_000_000_000,
        series: 'btc-15m',
        strategy_id: 'sha:late',
        deploy_epoch: 3,
        deployment_id: 'sha:late:3',
        template_id: 'late-closer',
        inverse: false,
      },
      runtime: {
        current_leg: 'waiting',
        armed_triggers: ['decided_up', 'decided_down'],
        paused: false,
        stopping: false,
        failure_reason: null,
        activations: 0,
        log: ['[start] leg waiting'],
      },
    }
    let deploys = 0
    server.use(
      http.post(
        `${baseUrl}/api/accounts/${address}/templates/late-closer/deploy`,
        async ({ request }) => {
          deploys += 1
          expect(request.headers.get('idempotency-key')).toBe('deploy-1')
          expect(await request.json()).toEqual({
            allocation_tenths: 50_000,
            inverse: false,
            series: 'btc-15m',
          })
          return deploys === 1
            ? new HttpResponse(null, { status: 503 })
            : HttpResponse.json(detail, { status: 201 })
        }
      ),
      http.post(`${baseUrl}/api/accounts`, () => new HttpResponse(null, { status: 503 }))
    )
    const client = createRustExchangeClient({ baseUrl, sleep: () => Promise.resolve() })
    const deployed = await client.deployTemplate(
      address,
      'late-closer',
      { allocation_tenths: 50_000, inverse: false, series: 'btc-15m' },
      'deploy-1'
    )
    expect(deploys).toBe(2)
    expect(deployed.ok && deployed.data.strategy.template_id).toBe('late-closer')
    expect(deployed.ok && deployed.data.runtime?.current_leg).toBe('waiting')
    // A mutation without a key is attempted exactly once.
    const created = await client.createAccount('jet')
    expect(created.ok).toBe(false)
    if (!created.ok) expect(created.error.status).toBe(503)
  })

  it('refuses a deploy the exchange rejects without retrying and lists strategies', async () => {
    let deploys = 0
    server.use(
      http.post(`${baseUrl}/api/accounts/${address}/templates/nope/deploy`, () => {
        deploys += 1
        return HttpResponse.json(
          { error: 'template_not_found', message: 'not listed' },
          { status: 404 }
        )
      }),
      http.get(`${baseUrl}/api/accounts/${address}/strategies`, () =>
        HttpResponse.json({
          strategies: [
            {
              id: `0x${'cd'.repeat(20)}`,
              kind: 'strategy',
              name: 'Momentum Rider',
              status: 'paused',
              created_at_ms: 1,
              home_series: 'btc-2m',
              trades_this_market: 2,
              series_positions: [
                {
                  series: 'btc-2m',
                  market_id: 9,
                  side: 'no',
                  size_shares: 28,
                  signed_position: -28,
                  avg_entry_yes_tenths: 300,
                  cost_basis_tenths: 19_600,
                  market_pnl_tenths: -400,
                  trades_this_market: 2,
                },
              ],
              balance_tenths: 5_400,
              locked_tenths: 0,
              equity_tenths: 24_600,
              net_contributed_tenths: 25_000,
              lifetime_pnl_tenths: -400,
              lifetime_realized_pnl_tenths: 0,
            },
          ],
        })
      )
    )
    const client = createRustExchangeClient({ baseUrl, sleep: () => Promise.resolve() })
    const refused = await client.deployTemplate(
      address,
      'nope',
      { allocation_tenths: 50_000, inverse: true, series: 'btc-2m' },
      'deploy-2'
    )
    expect(deploys).toBe(1)
    expect(!refused.ok && refused.error.message).toBe('template_not_found: not listed')
    const list = await client.listStrategies(address)
    expect(list.ok && list.data.strategies[0]?.series_positions[0]?.signed_position).toBe(-28)
    expect(list.ok && list.data.strategies[0]?.status).toBe('paused')
  })

  it('deploys a strategy on a funded sub-account and surfaces a compile refusal', async () => {
    const child = `0x${'34'.repeat(20)}`
    const envelope = { allocation: '100', loss_budget: '100', max_order_size: '2' }
    const detail = {
      id: child,
      kind: 'strategy',
      name: 'Threshold Entry',
      status: 'active',
      home_series: 'btc-2m',
      trades_this_market: 0,
      strategy: { series: 'btc-2m', strategy_id: 'Strategy_abc', deployment_id: 'Strategy_abc:1' },
      runtime: { current_leg: 'hunting' },
    }
    let calls = 0
    server.use(
      http.post(`${baseUrl}/api/accounts/${address}/strategies`, async ({ request }) => {
        calls += 1
        const body = await request.json()
        if (calls === 1) {
          expect(body).toEqual({
            allocation_tenths: 100_000,
            name: 'Threshold Entry',
            strategy: { version: '0.3' },
            envelope,
          })
          return HttpResponse.json(detail, { status: 201 })
        }
        return HttpResponse.json(
          { error: 'compile_failed', message: 'strategy failed to compile' },
          { status: 400 }
        )
      })
    )
    const client = createRustExchangeClient({ baseUrl, retry: { attempts: 1 } })
    const deployed = await client.createStrategy(address, {
      allocation_tenths: 100_000,
      name: 'Threshold Entry',
      strategy: { version: '0.3' },
      envelope,
    })
    expect(deployed.ok && deployed.data.id).toBe(child)
    const refused = await client.createStrategy(address, {
      allocation_tenths: 100_000,
      strategy: {},
      envelope,
    })
    expect(!refused.ok && refused.error.message).toBe('compile_failed: strategy failed to compile')
  })
})
