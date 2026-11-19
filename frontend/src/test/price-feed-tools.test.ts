import { noopObserve } from '@mastra/core/tools'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import { createPriceFeedTools } from '@/mastra/capabilities/price-feeds/tools'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())
const symbol = 'Crypto.BTC/USD'
const tools = createPriceFeedTools('http://feeds.test')

async function live(ageMs: number) {
  server.use(
    http.get(
      'http://feeds.test/v1/stream/prices',
      () =>
        new HttpResponse(
          `data: ${JSON.stringify({ v: 1, symbol, provider: 'PYTH_LAZER', mantissa: '9007199254740993', expo: -8, publishTimeUs: (Date.now() - ageMs) * 1000 })}\n\n`,
          { headers: { 'content-type': 'text/event-stream' } }
        )
    )
  )
  return tools.getLivePrice.execute?.({ symbol }, { observe: noopObserve })
}

describe('Kyros price tools', () => {
  it('includes observation provenance while preserving the exact price', async () => {
    expect(await live(100)).toMatchObject({
      status: 'ok',
      source: 'Kyros Price Feeds',
      provider: 'PYTH_LAZER',
      mantissa: '9007199254740993',
      expo: -8,
      observedAt: expect.any(String),
    })
  })
  it.each([6000, -2000])('withholds a current quote when observation age is %i ms', async ageMs => {
    const quote = await live(ageMs)
    expect(quote).toMatchObject({ status: 'unavailable' })
    expect(quote).not.toHaveProperty('mantissa')
  })
  it('fails closed without configuration', async () => {
    expect(
      await createPriceFeedTools(undefined).getLivePrice.execute?.(
        { symbol },
        { observe: noopObserve }
      )
    ).toMatchObject({
      status: 'unavailable',
    })
  })
  it('reports missing coverage instead of quoting carried-forward samples', async () => {
    server.use(
      http.get('http://feeds.test/v1/prices', () =>
        HttpResponse.json({
          s: 'ok',
          points: [{ t: 100, mantissa: '1', expo: 0 }],
          firstTickAtUs: null,
          lastTickAtUs: null,
        })
      )
    )
    expect(
      await tools.getPriceHistory.execute?.({ symbol, minutes: 60 }, { observe: noopObserve })
    ).toMatchObject({
      status: 'unavailable',
    })
  })
})
