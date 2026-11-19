import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import { marketSystemMessage } from '@/mastra/market-context'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

const series = [
  { series: 'btc-5m', duration_secs: 300, current_market_id: 1, opens_at_ms: 1, closes_at_ms: 2 },
  { series: 'btc-15m', duration_secs: 900, current_market_id: 1, opens_at_ms: 1, closes_at_ms: 2 },
]

describe('marketSystemMessage', () => {
  it('names the page’s market, its round length and its sides so Kyros never asks', async () => {
    server.use(http.get('http://exchange.test/api/series', () => HttpResponse.json(series)))
    await expect(marketSystemMessage('http://exchange.test', 'btc-15m')).resolves.toBe(
      'This chat is open on the 15-minute BTC up/down market (btc-15m). Its two sides are up and down; the book quotes up, and down is one dollar minus up. Every strategy built in this chat trades this market and no other. Never ask which market the user means; call it "the 15-minute BTC up/down market" when it matters.'
    )
  })

  it('still fixes the market when the exchange cannot be read', async () => {
    server.use(http.get('http://exchange.test/api/series', () => HttpResponse.error()))
    await expect(marketSystemMessage('http://exchange.test', 'btc-5m')).resolves.toMatch(
      /open on this market \(btc-5m\)\. Every strategy built in this chat trades this market/
    )
  })

  it('says no market is selected for a standalone chat', async () => {
    await expect(marketSystemMessage(undefined, 'standalone-chat')).resolves.toMatch(
      /no market is selected/
    )
  })
})
