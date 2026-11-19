import { noopObserve } from '@mastra/core/tools'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'

import { createExchangeTools } from '@/mastra/capabilities/exchange/tools'
import { createChatContext } from '@/mastra/request-context'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

const baseUrl = 'http://exchange.test'
const address = `0x${'ab'.repeat(20)}`
const tools = createExchangeTools(baseUrl)
const onMarket = (marketSlug: string, accountAddress?: string) => ({
  observe: noopObserve,
  requestContext: createChatContext({
    marketSlug,
    resourceId: 'r',
    ...(accountAddress === undefined ? {} : { accountAddress }),
  }),
})

const round = (id: number, strike: number, close: number, outcome: 'yes' | 'no') => ({
  id,
  strike_btc_usd: strike,
  opened_at: id * 120,
  closes_at: id * 120 + 120,
  status: { kind: 'resolved', outcome, close_price_btc_usd: close },
})

describe('Kyros exchange tools', () => {
  it('reads the round from the market the chat was opened from, in cents', async () => {
    let requested = ''
    server.use(
      http.get(`${baseUrl}/api/price/history`, ({ request }) => {
        requested = new URL(request.url).searchParams.get('series') ?? ''
        return HttpResponse.json({
          type: 'snapshot',
          series: 'btc-15m',
          duration_secs: 900,
          market_id: 4,
          strike: 72_754.14,
          opens_at_ms: 1,
          closes_at_ms: Date.now() + 47_000,
          ticks: [{ ts_ms: 2, price: 72_783.05 }],
        })
      }),
      http.get(`${baseUrl}/api/book`, () =>
        HttpResponse.json({ bids: [{ price: 610, qty: 4 }], asks: [{ price: 630, qty: 2 }] })
      ),
      http.get(`${baseUrl}/api/market/stats`, () =>
        HttpResponse.json({
          series: 'btc-15m',
          duration_secs: 900,
          market_id: 4,
          opened_at_ms: 1,
          closes_at_ms: 2,
          total_volume_tenths: 2_365_000,
          open_interest_contracts: 1512,
          total_locked_tenths: 1_512_000,
          total_trades: 29,
          unique_traders: 28,
          largest_trade: null,
        })
      )
    )
    const market = await tools.getMarket.execute?.({}, onMarket('btc-15m'))
    expect(requested).toBe('btc-15m')
    expect(market).toMatchObject({
      status: 'ok',
      series: 'btc-15m',
      spot_usd: 72_783.05,
      round: { market_id: 4, strike_usd: 72_754.14 },
      yes: { bid_cents: 61, ask_cents: 63, spread_cents: 2 },
      stats: { volume_cents: 236_500, open_interest_lots: 1512, trades: 29, traders: 28 },
    })
    const seconds = (market as { round: { seconds_remaining: number } }).round.seconds_remaining
    expect(seconds).toBeGreaterThan(40)
  })

  it('falls back to the default series off a market page and fails closed without an account', async () => {
    server.use(
      http.get(`${baseUrl}/api/book`, ({ request }) => {
        expect(new URL(request.url).searchParams.get('series')).toBe('btc-5m')
        expect(new URL(request.url).searchParams.get('depth')).toBe('2')
        return HttpResponse.json({
          bids: [
            { price: 610, qty: 4 },
            { price: 600, qty: 4 },
          ],
          asks: [{ price: 630, qty: 2 }],
        })
      })
    )
    const book = await tools.getBook.execute?.({ depth: 2 }, onMarket('standalone-chat'))
    expect(book).toMatchObject({
      status: 'ok',
      series: 'btc-5m',
      spread_cents: 2,
      imbalance: 0.6,
      bids: [
        { price_cents: 61, lots: 4 },
        { price_cents: 60, lots: 4 },
      ],
    })
    expect(await tools.getPositions.execute?.({}, onMarket('btc-5m'))).toMatchObject({
      status: 'unavailable',
      reason: expect.stringContaining('not signed in'),
    })
    expect(await tools.getTrades.execute?.({ scope: 'mine' }, onMarket('btc-5m'))).toMatchObject({
      status: 'unavailable',
    })
  })

  it('summarises rounds newest first with the streak, and prices in dollars', async () => {
    server.use(
      http.get(`${baseUrl}/api/markets/history`, () =>
        HttpResponse.json([
          { id: 5, strike_btc_usd: 100, opened_at: 600, closes_at: 720, status: { kind: 'open' } },
          round(4, 100, 130, 'yes'),
          round(3, 100, 101, 'yes'),
          round(2, 100, 90, 'no'),
        ])
      ),
      http.get(`${baseUrl}/api/price/history`, () =>
        HttpResponse.json({
          type: 'snapshot',
          series: 'btc-5m',
          duration_secs: 300,
          market_id: null,
          strike: null,
          opens_at_ms: null,
          closes_at_ms: null,
          ticks: [
            { ts_ms: 1, price: 100 },
            { ts_ms: 2, price: 90 },
            { ts_ms: 3, price: 105 },
          ],
        })
      )
    )
    expect(await tools.getRounds.execute?.({ count: 3 }, onMarket('btc-5m'))).toMatchObject({
      status: 'ok',
      up: 2,
      down: 1,
      streak: { outcome: 'up', length: 2 },
      rounds: [{ market_id: 4, move_usd: 30 }, { market_id: 3 }, { market_id: 2, outcome: 'down' }],
    })
    expect(await tools.getExchangePriceHistory.execute?.({}, onMarket('btc-5m'))).toMatchObject({
      status: 'ok',
      round: null,
      first_usd: 100,
      last_usd: 105,
      low_usd: 90,
      high_usd: 105,
    })
  })

  it('reads positions and the user trade log for the signed-in account', async () => {
    server.use(
      http.get(`${baseUrl}/api/accounts/${address}/positions`, () =>
        HttpResponse.json({
          type: 'position_snapshot',
          as_of_unix_ms: 9,
          series: 'btc-5m',
          market_id: 3,
          address,
          side: 'yes',
          size_shares: 4,
          signed_position: 4,
          avg_entry_yes_tenths: 580,
          cost_basis_tenths: 2320,
          mark_yes_tenths: 620,
          unrealized_pnl_tenths: 160,
          realized_pnl_this_market_tenths: 0,
          balance_tenths: 997_680,
          locked_tenths: 2320,
          series_positions: [
            {
              series: 'btc-5m',
              market_id: 3,
              side: 'yes',
              size_shares: 4,
              signed_position: 4,
              avg_entry_yes_tenths: 580,
              cost_basis_tenths: 2320,
              mark_yes_tenths: 620,
              mark_value_tenths: 2480,
              unrealized_pnl_tenths: 160,
              market_pnl_tenths: 160,
            },
          ],
        })
      ),
      http.get(`${baseUrl}/api/accounts/${address}/trades`, () =>
        HttpResponse.json({
          trades: [
            {
              series: 'btc-5m',
              order_id: 9,
              ts: 10,
              side: 'yes',
              qty_requested: 4,
              filled_qty: 4,
              avg_price: 580,
              source: 'ui',
              order_type: 'market',
              status: 'filled',
              market_id: 3,
              realized_pnl_tenths: 0,
              settled: false,
            },
          ],
        })
      )
    )
    expect(await tools.getPositions.execute?.({}, onMarket('btc-5m', address))).toMatchObject({
      status: 'ok',
      balance_cents: 99_768,
      positions: [{ series: 'btc-5m', signed_lots: 4, avg_entry_cents: 58, mark_cents: 62 }],
    })
    expect(
      await tools.getTrades.execute?.({ scope: 'mine' }, onMarket('btc-5m', address))
    ).toMatchObject({
      status: 'ok',
      orders: [{ bought: 'YES', filled_lots: 4, avg_price_cents: 58, status: 'filled' }],
    })
  })

  it('fails closed without configuration', async () => {
    expect(
      await createExchangeTools(undefined).getMarket.execute?.({}, onMarket('btc-5m'))
    ).toEqual({ status: 'unavailable', reason: 'The exchange is not configured.' })
  })
})
